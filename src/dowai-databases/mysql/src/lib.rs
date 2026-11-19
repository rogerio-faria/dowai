//! Adapter MySQL como origem e destino (ENT-034, plano §10.1).
//!
//! Segue o contrato do adapter PostgreSQL (`SqlAdapter`), com capabilities próprias e verdadeiras
//! para o MySQL ([`MYSQL_CAPABILITIES`]). Os métodos têm os mesmos nomes do contrato, mas são
//! inerentes: a trait `SqlAdapter` expõe `postgres::types::ToSql` nos parâmetros, então este
//! adapter usa [`Value`] do driver `mysql`.
//!
//! Diferenças reais em relação ao PostgreSQL, refletidas no código e nos testes:
//! - DDL não é transacional: `CREATE/DROP/ALTER/TRUNCATE/RENAME` causam commit implícito. Por
//!   isso o adapter recusa DDL (exceto de tabela temporária) dentro de uma transação aberta
//!   ([`MysqlError::Unsupported`]), em vez de deixar o commit implícito acontecer em silêncio.
//! - Não existe `ON CONFLICT`: o upsert usa `INSERT ... SELECT ... ON DUPLICATE KEY UPDATE`, que
//!   dispara em *qualquer* índice único do destino, não só nas chaves informadas.
//! - Tabelas temporárias não somem no `COMMIT`: o adapter as remove explicitamente ao confirmar
//!   ou desfazer a transação.
//! - Transações abrangem somente um banco. Não há atomicidade entre bancos (plano §10.2).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use dowai_postgres::{
    Batch, Capabilities, ColumnInfo, DatasetCell, Materialization, MergeCounters, RowCount,
    StreamLimits, TlsMode,
};
use mysql::prelude::Queryable;
pub use mysql::Value;
use mysql::{Conn, OptsBuilder, SslOpts};

/// Limite de parâmetros do protocolo MySQL por instrução preparada.
pub const WRITE_MAX_PARAMS: usize = 65_535;
/// Limite superior de linhas por lote de leitura.
pub const STREAM_BATCH_ROWS_MAX: u32 = 1_000_000;

const CONNECT_TIMEOUT_MAX: Duration = Duration::from_secs(600);
const QUERY_TIMEOUT_MAX: Duration = Duration::from_secs(86_400);
/// Comprimento máximo de identificador no MySQL.
const IDENTIFIER_MAX: usize = 64;

/// Capabilities reais do adapter MySQL. Só declara o que os testes de contrato verificam.
pub const MYSQL_CAPABILITIES: Capabilities = Capabilities {
    // DDL causa commit implícito; o adapter recusa DDL dentro de transação.
    transactional_ddl: false,
    identifier_quoting: true,
    schemas: false,
    staging: true,
    bulk_load: false,
    stream: true,
    merge_upsert: true,
    tls: true,
    parameters: true,
    row_count: true,
    cancellation: false,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MysqlError {
    InvalidProfile(&'static str),
    InvalidIdentifier,
    InsecureTlsDisabled,
    DestructiveApprovalRequired,
    MissingPassword,
    TlsConfigurationFailed,
    ConnectionFailed,
    QueryFailed,
    /// Falha transitória: conexão perdida, timeout de lock/consulta ou deadlock. A transação
    /// foi desfeita pelo servidor, então nada foi aplicado.
    TransientFailure,
    InvalidTransactionState,
    InvalidStreamLimit,
    InflightLimitExceeded,
    InvalidWriteSpec(&'static str),
    DuplicateMergeKey,
    SchemaMismatch,
    /// Capacidade que o MySQL não oferece com a mesma garantia (ex.: DDL em transação).
    Unsupported(&'static str),
}

impl MysqlError {
    /// `true` apenas para falhas que o runtime pode repetir.
    pub fn is_transient(self) -> bool {
        matches!(self, Self::TransientFailure)
    }
}

impl std::fmt::Display for MysqlError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidProfile(field) => write!(formatter, "invalid MySQL profile: {field}"),
            Self::InvalidIdentifier => formatter.write_str("invalid MySQL identifier"),
            Self::InsecureTlsDisabled => {
                formatter.write_str("TLS may only be disabled for a local MySQL host")
            }
            Self::DestructiveApprovalRequired => {
                formatter.write_str("replacing an existing relation requires explicit approval")
            }
            Self::MissingPassword => {
                formatter.write_str("required password environment variable is missing")
            }
            Self::TlsConfigurationFailed => formatter.write_str("MySQL TLS configuration failed"),
            Self::ConnectionFailed => formatter.write_str("MySQL connection failed"),
            Self::QueryFailed => formatter.write_str("MySQL query failed"),
            Self::TransientFailure => {
                formatter.write_str("MySQL transient failure; the operation may be retried")
            }
            Self::InvalidTransactionState => formatter.write_str("invalid MySQL transaction state"),
            Self::InvalidStreamLimit => formatter.write_str("invalid MySQL stream limits"),
            Self::InflightLimitExceeded => {
                formatter.write_str("MySQL batch exceeds the in-flight byte limit")
            }
            Self::InvalidWriteSpec(reason) => {
                write!(formatter, "invalid MySQL write specification: {reason}")
            }
            Self::DuplicateMergeKey => {
                formatter.write_str("MySQL merge staging has duplicate keys")
            }
            Self::SchemaMismatch => {
                formatter.write_str("MySQL full refresh schema differs from the existing relation")
            }
            Self::Unsupported(reason) => {
                write!(formatter, "unsupported by MySQL adapter: {reason}")
            }
        }
    }
}

impl std::error::Error for MysqlError {}

/// Perfil de conexão; senhas literais são omitidas da saída de depuração.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MysqlProfile {
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

impl MysqlProfile {
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

    fn resolve_password(&self) -> Result<String, MysqlError> {
        match &self.password {
            Some(password) => Ok(password.0.clone()),
            None => std::env::var(&self.password_env_var).map_err(|_| MysqlError::MissingPassword),
        }
    }

    pub fn tls_mode(&self) -> TlsMode {
        self.tls_mode
    }

    pub fn set_tls_mode(&mut self, tls_mode: TlsMode) {
        self.tls_mode = tls_mode;
    }

    pub fn set_connect_timeout(&mut self, timeout: Duration) {
        self.connect_timeout = timeout;
    }

    /// `max_execution_time` da sessão (vale para `SELECT`; o MySQL não aplica a DML).
    pub fn set_query_timeout(&mut self, timeout: Option<Duration>) {
        self.query_timeout = timeout;
    }

    pub fn validate(&self) -> Result<(), MysqlError> {
        for (field, value) in [
            ("host", self.host.as_str()),
            ("user", self.user.as_str()),
            ("database", self.database.as_str()),
        ] {
            if value.trim().is_empty() || value.chars().any(char::is_control) {
                return Err(MysqlError::InvalidProfile(field));
            }
        }
        if self.host.chars().any(char::is_whitespace) {
            return Err(MysqlError::InvalidProfile("host"));
        }
        if self.port == 0 {
            return Err(MysqlError::InvalidProfile("port"));
        }
        if self.password.is_none() && !valid_env_var_name(&self.password_env_var) {
            return Err(MysqlError::InvalidProfile("password environment variable"));
        }
        if self.tls_mode == TlsMode::Disabled && !is_local_host(&self.host) {
            return Err(MysqlError::InsecureTlsDisabled);
        }
        if self.connect_timeout.is_zero() || self.connect_timeout > CONNECT_TIMEOUT_MAX {
            return Err(MysqlError::InvalidProfile("connect timeout"));
        }
        if let Some(timeout) = self.query_timeout {
            if timeout.is_zero() || timeout > QUERY_TIMEOUT_MAX {
                return Err(MysqlError::InvalidProfile("query timeout"));
            }
        }
        Ok(())
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
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Escapa um identificador com crases. Rejeita vazio, controle, NUL e mais de 64 caracteres.
pub fn quote_identifier(identifier: &str) -> Result<String, MysqlError> {
    if identifier.is_empty()
        || identifier.chars().count() > IDENTIFIER_MAX
        || identifier.chars().any(char::is_control)
    {
        return Err(MysqlError::InvalidIdentifier);
    }
    Ok(format!("`{}`", identifier.replace('`', "``")))
}

fn quoted_list(names: &[String]) -> Result<String, MysqlError> {
    Ok(names
        .iter()
        .map(|name| quote_identifier(name))
        .collect::<Result<Vec<_>, _>>()?
        .join(", "))
}

/// Códigos de erro do servidor tratados como transitórios: 1205 (lock wait timeout), 1213
/// (deadlock), 3024 (max_execution_time), 1317 (consulta interrompida), 2006/2013 (conexão
/// perdida).
fn classify(error: mysql::Error) -> MysqlError {
    match &error {
        mysql::Error::MySqlError(server) => match server.code {
            1205 | 1213 | 1317 | 3024 | 2006 | 2013 => MysqlError::TransientFailure,
            _ => MysqlError::QueryFailed,
        },
        mysql::Error::IoError(_) | mysql::Error::CodecError(_) => MysqlError::TransientFailure,
        _ => MysqlError::QueryFailed,
    }
}

fn server_code(error: &mysql::Error) -> Option<u16> {
    match error {
        mysql::Error::MySqlError(server) => Some(server.code),
        _ => None,
    }
}

/// `true` para DDL que causa commit implícito. `CREATE/DROP TEMPORARY TABLE` não causa.
fn causes_implicit_commit(sql: &str) -> bool {
    let words: Vec<String> = sql
        .split_whitespace()
        .take(2)
        .map(str::to_ascii_uppercase)
        .collect();
    let first = words.first().map(String::as_str).unwrap_or("");
    let second = words.get(1).map(String::as_str).unwrap_or("");
    match first {
        "CREATE" | "DROP" => second != "TEMPORARY",
        "ALTER" | "TRUNCATE" | "RENAME" | "LOCK" | "UNLOCK" | "BEGIN" | "START" => true,
        _ => false,
    }
}

pub struct MysqlAdapter {
    conn: Conn,
    in_transaction: bool,
    /// Tabelas temporárias de staging a remover ao fim da transação.
    staging: Vec<String>,
}

#[derive(Clone, Copy)]
enum RelationKind {
    Table,
    View,
}

impl MysqlAdapter {
    pub fn connect(profile: &MysqlProfile) -> Result<Self, MysqlError> {
        profile.validate()?;
        let password = profile.resolve_password()?;
        let mut init = vec![
            // Escrita estrita: valor que não cabe é erro, nunca truncamento silencioso.
            "SET SESSION sql_mode = CONCAT_WS(',', @@SESSION.sql_mode, 'STRICT_ALL_TABLES')"
                .to_owned(),
        ];
        if let Some(timeout) = profile.query_timeout {
            init.push(format!(
                "SET SESSION max_execution_time = {}",
                timeout.as_millis()
            ));
        }
        let builder = OptsBuilder::new()
            .ip_or_hostname(Some(profile.host.clone()))
            .tcp_port(profile.port)
            .user(Some(profile.user.clone()))
            .pass(Some(password))
            .db_name(Some(profile.database.clone()))
            .tcp_connect_timeout(Some(profile.connect_timeout))
            .prefer_socket(false)
            .ssl_opts(match profile.tls_mode {
                TlsMode::Required => Some(SslOpts::default()),
                TlsMode::Disabled => None,
            })
            .init(init);
        let conn = Conn::new(builder).map_err(|error| match error {
            mysql::Error::TlsError(_) => MysqlError::TlsConfigurationFailed,
            _ => MysqlError::ConnectionFailed,
        })?;
        Ok(Self {
            conn,
            in_transaction: false,
            staging: Vec::new(),
        })
    }

    pub fn capabilities(&self) -> Capabilities {
        MYSQL_CAPABILITIES
    }

    /// Confirma a conexão com consulta trivial e devolve as capabilities.
    pub fn test_connection(&mut self) -> Result<Capabilities, MysqlError> {
        self.conn
            .query_drop("SELECT 1")
            .map_err(classify)
            .map(|()| MYSQL_CAPABILITIES)
    }

    /// Builds a table or view from one SQL statement. MySQL DDL is not transactional, so an
    /// existing relation is swapped with a staged relation using one `RENAME TABLE` statement.
    pub fn materialize(
        &mut self,
        relation: &str,
        materialization: Materialization,
        query: &str,
        allow_replace: bool,
    ) -> Result<(), MysqlError> {
        self.materialize_with_mode(relation, materialization, query, allow_replace, false)
    }

    pub fn full_refresh(
        &mut self,
        relation: &str,
        materialization: Materialization,
        query: &str,
    ) -> Result<(), MysqlError> {
        self.materialize_with_mode(relation, materialization, query, true, true)
    }

    fn materialize_with_mode(
        &mut self,
        relation: &str,
        materialization: Materialization,
        query: &str,
        allow_replace: bool,
        check_schema: bool,
    ) -> Result<(), MysqlError> {
        let relation_q = quote_identifier(relation)?;
        if query.trim().is_empty() {
            return Err(MysqlError::QueryFailed);
        }
        let existing = self.relation_kind(relation)?;
        if existing.is_some() && !allow_replace {
            return Err(MysqlError::DestructiveApprovalRequired);
        }

        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| MysqlError::QueryFailed)?
            .as_nanos()
            % 1_000_000_000;
        let stage_name = format!("dw_stg_{}_{}", std::process::id(), suffix);
        let old_name = format!("dw_old_{}_{}", std::process::id(), suffix);
        let stage_q = quote_identifier(&stage_name)?;
        let old_q = quote_identifier(&old_name)?;
        let create = match materialization {
            Materialization::Table => format!("CREATE TABLE {stage_q} AS {query}"),
            Materialization::View => format!("CREATE VIEW {stage_q} AS {query}"),
        };
        self.conn.query_drop(create).map_err(classify)?;

        if check_schema && existing.is_some() {
            let current = self.introspect_table(relation)?;
            let candidate = self.introspect_table(&stage_name)?;
            if current != candidate {
                let _ = self
                    .conn
                    .query_drop(format!("DROP TABLE IF EXISTS {stage_q}"));
                let _ = self
                    .conn
                    .query_drop(format!("DROP VIEW IF EXISTS {stage_q}"));
                return Err(MysqlError::SchemaMismatch);
            }
        }

        let renamed = match existing {
            Some(_) => self.conn.query_drop(format!(
                "RENAME TABLE {relation_q} TO {old_q}, {stage_q} TO {relation_q}"
            )),
            None => self
                .conn
                .query_drop(format!("RENAME TABLE {stage_q} TO {relation_q}")),
        };
        if let Err(error) = renamed {
            let _ = self
                .conn
                .query_drop(format!("DROP TABLE IF EXISTS {stage_q}"));
            let _ = self
                .conn
                .query_drop(format!("DROP VIEW IF EXISTS {stage_q}"));
            return Err(classify(error));
        }

        if let Some(kind) = existing {
            let drop = match kind {
                RelationKind::Table => "DROP TABLE",
                RelationKind::View => "DROP VIEW",
            };
            self.conn
                .query_drop(format!("{drop} {old_q}"))
                .map_err(classify)?;
        }
        Ok(())
    }

    fn relation_kind(&mut self, relation: &str) -> Result<Option<RelationKind>, MysqlError> {
        quote_identifier(relation)?;
        let kind: Option<String> = self
            .conn
            .exec_first(
                "SELECT CAST(table_type AS CHAR) FROM information_schema.tables \
                 WHERE table_schema = DATABASE() AND table_name = ?",
                (relation,),
            )
            .map_err(classify)?;
        match kind.as_deref() {
            None => Ok(None),
            Some("BASE TABLE") => Ok(Some(RelationKind::Table)),
            Some("VIEW") => Ok(Some(RelationKind::View)),
            Some(_) => Err(MysqlError::Unsupported(
                "only tables and views can be materialized",
            )),
        }
    }

    /// Colunas da chave primária de `table`, na ordem da chave (vazio se não houver).
    pub fn primary_key(&mut self, table: &str) -> Result<Vec<String>, MysqlError> {
        quote_identifier(table)?;
        let rows: Vec<String> = self
            .conn
            .exec(
                "SELECT CAST(column_name AS CHAR) FROM information_schema.key_column_usage \
                 WHERE table_schema = DATABASE() AND table_name = ? \
                 AND constraint_name = 'PRIMARY' ORDER BY ordinal_position",
                (table,),
            )
            .map_err(classify)?;
        Ok(rows)
    }

    /// Colunas e tipos de `table` no banco corrente (identificador simples, sem schema).
    pub fn introspect_table(&mut self, table: &str) -> Result<Vec<ColumnInfo>, MysqlError> {
        quote_identifier(table)?;
        let rows: Vec<(String, String)> = self
            .conn
            .exec(
                "SELECT CAST(column_name AS CHAR) AS c, CAST(column_type AS CHAR) AS t \
                 FROM information_schema.columns \
                 WHERE table_schema = DATABASE() AND table_name = ? ORDER BY ordinal_position",
                (table,),
            )
            .map_err(classify)?;
        if rows.is_empty() {
            return Err(MysqlError::QueryFailed);
        }
        Ok(rows
            .into_iter()
            .map(|(name, data_type)| ColumnInfo { name, data_type })
            .collect())
    }

    /// Executa uma instrução com parâmetros vinculados e devolve as linhas afetadas. Dentro de
    /// transação, DDL com commit implícito é recusada.
    pub fn execute_sql(&mut self, sql: &str, params: &[Value]) -> Result<u64, MysqlError> {
        if self.in_transaction && causes_implicit_commit(sql) {
            return Err(MysqlError::Unsupported(
                "DDL and transaction-control statements cause an implicit commit in MySQL",
            ));
        }
        self.conn
            .exec_drop(sql, params.to_vec())
            .map_err(classify)?;
        Ok(self.conn.affected_rows())
    }

    /// Consulta que devolve todas as linhas decodificadas (uso em verificações e leituras
    /// pequenas; para volume use [`MysqlAdapter::stream_query`]).
    pub fn query_rows(
        &mut self,
        sql: &str,
        params: &[Value],
    ) -> Result<Vec<Vec<DatasetCell>>, MysqlError> {
        let result = self
            .conn
            .exec_iter(sql, params.to_vec())
            .map_err(classify)?;
        let types = column_types(&result);
        let mut rows = Vec::new();
        for row in result {
            let row = row.map_err(classify)?;
            rows.push(decode_row(row.unwrap(), &types));
        }
        Ok(rows)
    }

    pub fn begin_transaction(&mut self) -> Result<(), MysqlError> {
        if self.in_transaction {
            return Err(MysqlError::InvalidTransactionState);
        }
        self.conn
            .query_drop("START TRANSACTION")
            .map_err(classify)?;
        self.in_transaction = true;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<(), MysqlError> {
        if !self.in_transaction {
            return Err(MysqlError::InvalidTransactionState);
        }
        self.conn.query_drop("COMMIT").map_err(classify)?;
        self.in_transaction = false;
        self.drop_staging();
        Ok(())
    }

    pub fn rollback(&mut self) -> Result<(), MysqlError> {
        if !self.in_transaction {
            return Err(MysqlError::InvalidTransactionState);
        }
        let outcome = self.conn.query_drop("ROLLBACK").map_err(classify);
        self.in_transaction = false;
        self.drop_staging();
        outcome
    }

    fn drop_staging(&mut self) {
        for name in std::mem::take(&mut self.staging) {
            if let Ok(quoted) = quote_identifier(&name) {
                let _ = self
                    .conn
                    .query_drop(format!("DROP TEMPORARY TABLE IF EXISTS {quoted}"));
            }
        }
    }

    fn require_transaction(&self) -> Result<(), MysqlError> {
        if self.in_transaction {
            Ok(())
        } else {
            Err(MysqlError::InvalidTransactionState)
        }
    }

    /// Leitura em lotes. O servidor envia as linhas conforme o consumidor as pede (protocolo
    /// binário não bufferizado): nada além do lote pedido é decodificado. Recusa uso dentro de
    /// transação aberta, como o adapter PostgreSQL.
    pub fn stream_query<'a>(
        &'a mut self,
        sql: &str,
        params: &[Value],
        limits: StreamLimits,
    ) -> Result<MysqlBatchStream<'a>, MysqlError> {
        if limits.batch_rows == 0
            || limits.batch_rows > STREAM_BATCH_ROWS_MAX
            || limits.max_inflight_bytes == 0
        {
            return Err(MysqlError::InvalidStreamLimit);
        }
        if self.in_transaction {
            return Err(MysqlError::InvalidTransactionState);
        }
        let result = self
            .conn
            .exec_iter(sql, params.to_vec())
            .map_err(classify)?;
        let types = column_types(&result);
        let columns: Vec<(String, String)> = result
            .columns()
            .as_ref()
            .iter()
            .map(|column| {
                (
                    column.name_str().into_owned(),
                    format!("{:?}", column.column_type())
                        .trim_start_matches("MYSQL_TYPE_")
                        .to_ascii_lowercase(),
                )
            })
            .collect();
        if columns.is_empty() {
            return Err(MysqlError::QueryFailed);
        }
        Ok(MysqlBatchStream {
            result: Some(result),
            columns,
            types,
            limits,
            rows_read: 0,
            pending: None,
            failed: false,
        })
    }

    /// Cria a staging temporária com as mesmas colunas, defaults e índices de `target`. Exige
    /// transação. Removida por `commit` ou `rollback` (o MySQL não tem `ON COMMIT DROP`).
    pub fn create_staging(&mut self, target: &str, staging: &str) -> Result<(), MysqlError> {
        self.require_transaction()?;
        let target = quote_identifier(target)?;
        let staging_q = quote_identifier(staging)?;
        self.conn
            .query_drop(format!("CREATE TEMPORARY TABLE {staging_q} LIKE {target}"))
            .map_err(classify)?;
        self.staging.push(staging.to_owned());
        Ok(())
    }

    /// Insere um lote parametrizado na staging (uma linha por entrada, valores na ordem de
    /// `columns`). Exige transação. A staging herda os índices únicos do destino: linha repetida
    /// por chave única resulta em [`MysqlError::DuplicateMergeKey`].
    pub fn write_staging_batch(
        &mut self,
        staging: &str,
        columns: &[String],
        rows: &[Vec<Value>],
    ) -> Result<u64, MysqlError> {
        self.require_transaction()?;
        validate_batch_shape(columns, rows.iter().map(Vec::len))?;
        if rows.is_empty() {
            return Ok(0);
        }
        let staging_q = quote_identifier(staging)?;
        let columns_sql = quoted_list(columns)?;
        let per_row = columns.len();
        let rows_per_statement = (WRITE_MAX_PARAMS / per_row).max(1);
        let mut inserted = 0;
        for chunk in rows.chunks(rows_per_statement) {
            let one = format!("({})", vec!["?"; per_row].join(", "));
            let values = vec![one; chunk.len()].join(", ");
            let sql = format!("INSERT INTO {staging_q} ({columns_sql}) VALUES {values}");
            let params: Vec<Value> = chunk.iter().flat_map(|row| row.iter().cloned()).collect();
            self.conn.exec_drop(sql, params).map_err(|error| {
                if server_code(&error) == Some(1062) {
                    MysqlError::DuplicateMergeKey
                } else {
                    classify(error)
                }
            })?;
            inserted += self.conn.affected_rows();
        }
        Ok(inserted)
    }

    /// Upsert da staging no destino por `INSERT ... SELECT ... ON DUPLICATE KEY UPDATE`.
    /// Exige transação. `inserted` conta chaves novas e `updated` as demais linhas da staging
    /// (linha idêntica à existente conta como `updated`, como no adapter PostgreSQL). O
    /// destino precisa de índice único sobre `keys`; o MySQL também atualiza por qualquer outro
    /// índice único do destino.
    pub fn merge(
        &mut self,
        staging: &str,
        target: &str,
        columns: &[String],
        keys: &[String],
    ) -> Result<MergeCounters, MysqlError> {
        self.require_transaction()?;
        validate_merge_spec(columns, keys)?;
        let staging_q = quote_identifier(staging)?;
        let target_q = quote_identifier(target)?;
        let columns_sql = quoted_list(columns)?;
        let keys_sql = quoted_list(keys)?;

        let duplicated: Option<u8> = self
            .conn
            .query_first(format!(
                "SELECT 1 FROM {staging_q} GROUP BY {keys_sql} HAVING COUNT(*) > 1 LIMIT 1"
            ))
            .map_err(classify)?;
        if duplicated.is_some() {
            return Err(MysqlError::DuplicateMergeKey);
        }
        let staged: u64 = self
            .conn
            .query_first(format!("SELECT COUNT(*) FROM {staging_q}"))
            .map_err(classify)?
            .unwrap_or(0);
        let join = keys
            .iter()
            .map(|key| {
                let key = quote_identifier(key)?;
                Ok(format!("t.{key} = s.{key}"))
            })
            .collect::<Result<Vec<_>, MysqlError>>()?
            .join(" AND ");
        let inserted: u64 = self
            .conn
            .query_first(format!(
                "SELECT COUNT(*) FROM {staging_q} s WHERE NOT EXISTS \
                 (SELECT 1 FROM {target_q} t WHERE {join})"
            ))
            .map_err(classify)?
            .unwrap_or(0);

        let updates: Vec<String> = columns
            .iter()
            .filter(|column| !keys.contains(column))
            .map(|column| {
                let quoted = quote_identifier(column)?;
                Ok(format!("{quoted} = VALUES({quoted})"))
            })
            .collect::<Result<_, MysqlError>>()?;
        let update_sql = if updates.is_empty() {
            // Só chaves: atribuição nula para manter a semântica "não duplica".
            let first = quote_identifier(&keys[0])?;
            format!("{first} = {first}")
        } else {
            updates.join(", ")
        };
        self.conn
            .query_drop(format!(
                "INSERT INTO {target_q} ({columns_sql}) SELECT {columns_sql} FROM {staging_q} \
                 ON DUPLICATE KEY UPDATE {update_sql}"
            ))
            .map_err(classify)?;
        Ok(MergeCounters {
            inserted,
            updated: staged.saturating_sub(inserted),
        })
    }

    /// Copia a staging para o destino sem conflito (`append`). Exige transação.
    pub fn append_staging(
        &mut self,
        staging: &str,
        target: &str,
        columns: &[String],
    ) -> Result<u64, MysqlError> {
        self.require_transaction()?;
        validate_batch_shape(columns, std::iter::empty())?;
        let staging_q = quote_identifier(staging)?;
        let target_q = quote_identifier(target)?;
        let columns_sql = quoted_list(columns)?;
        self.conn
            .query_drop(format!(
                "INSERT INTO {target_q} ({columns_sql}) SELECT {columns_sql} FROM {staging_q}"
            ))
            .map_err(classify)?;
        Ok(self.conn.affected_rows())
    }

    /// Remove todas as linhas do destino com `DELETE` (transacional; `TRUNCATE` não seria).
    pub fn delete_all(&mut self, target: &str) -> Result<u64, MysqlError> {
        self.require_transaction()?;
        let target_q = quote_identifier(target)?;
        self.conn
            .query_drop(format!("DELETE FROM {target_q}"))
            .map_err(classify)?;
        Ok(self.conn.affected_rows())
    }

    /// Indica se `target` tem índice único cujas colunas são exatamente `keys` (sem prefixo
    /// nem expressão). Pré-condição de `upsert`.
    pub fn has_unique_key(&mut self, target: &str, keys: &[String]) -> Result<bool, MysqlError> {
        quote_identifier(target)?;
        let rows: Vec<(String, Option<String>, Option<u64>)> = self
            .conn
            .exec(
                "SELECT CAST(index_name AS CHAR), CAST(column_name AS CHAR), sub_part \
                 FROM information_schema.statistics \
                 WHERE table_schema = DATABASE() AND table_name = ? AND non_unique = 0 \
                 ORDER BY index_name, seq_in_index",
                (target,),
            )
            .map_err(classify)?;
        let mut expected: Vec<&str> = keys.iter().map(String::as_str).collect();
        expected.sort_unstable();
        let mut indexes: std::collections::BTreeMap<String, Option<Vec<String>>> =
            std::collections::BTreeMap::new();
        for (index, column, sub_part) in rows {
            let entry = indexes.entry(index).or_insert_with(|| Some(Vec::new()));
            match (column, sub_part, entry.as_mut()) {
                (Some(column), None, Some(columns)) => columns.push(column),
                _ => *entry = None,
            }
        }
        Ok(indexes.values().flatten().any(|columns| {
            let mut names: Vec<&str> = columns.iter().map(String::as_str).collect();
            names.sort_unstable();
            names == expected
        }))
    }
}

fn validate_merge_spec(columns: &[String], keys: &[String]) -> Result<(), MysqlError> {
    if columns.is_empty() || keys.is_empty() {
        return Err(MysqlError::InvalidWriteSpec(
            "columns and keys must not be empty",
        ));
    }
    if has_duplicates(columns) || has_duplicates(keys) {
        return Err(MysqlError::InvalidWriteSpec("duplicate column or key name"));
    }
    if keys.iter().any(|key| !columns.contains(key)) {
        return Err(MysqlError::InvalidWriteSpec(
            "merge key is not a written column",
        ));
    }
    Ok(())
}

fn validate_batch_shape(
    columns: &[String],
    row_lengths: impl Iterator<Item = usize>,
) -> Result<(), MysqlError> {
    if columns.is_empty() {
        return Err(MysqlError::InvalidWriteSpec(
            "batch must declare at least one column",
        ));
    }
    for length in row_lengths {
        if length != columns.len() {
            return Err(MysqlError::InvalidWriteSpec(
                "row length differs from column count",
            ));
        }
    }
    Ok(())
}

fn has_duplicates(names: &[String]) -> bool {
    let mut seen = std::collections::BTreeSet::new();
    names.iter().any(|name| !seen.insert(name.as_str()))
}

fn column_types<P: mysql::prelude::Protocol>(
    result: &mysql::QueryResult<'_, '_, '_, P>,
) -> Vec<mysql::consts::ColumnType> {
    result
        .columns()
        .as_ref()
        .iter()
        .map(|column| column.column_type())
        .collect()
}

fn decode_row(values: Vec<Value>, types: &[mysql::consts::ColumnType]) -> Vec<DatasetCell> {
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| decode_cell(value, types.get(index).copied()))
        .collect()
}

fn decode_cell(value: Value, column_type: Option<mysql::consts::ColumnType>) -> DatasetCell {
    use mysql::consts::ColumnType;
    match value {
        Value::NULL => DatasetCell::Null,
        Value::Int(number) => DatasetCell::Int(number),
        Value::UInt(number) => i64::try_from(number)
            .map(DatasetCell::Int)
            .unwrap_or_else(|_| DatasetCell::Text(number.to_string())),
        Value::Float(number) => DatasetCell::Float(f64::from(number)),
        Value::Double(number) => DatasetCell::Float(number),
        Value::Bytes(bytes) => match String::from_utf8(bytes) {
            Ok(text) => DatasetCell::Text(text),
            Err(error) => DatasetCell::Text(
                error
                    .into_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            ),
        },
        Value::Date(year, month, day, hour, minute, second, micro) => {
            let date_only = matches!(
                column_type,
                Some(ColumnType::MYSQL_TYPE_DATE | ColumnType::MYSQL_TYPE_NEWDATE)
            );
            let mut text = format!("{year:04}-{month:02}-{day:02}");
            if !date_only {
                text.push_str(&format!(" {hour:02}:{minute:02}:{second:02}"));
                if micro != 0 {
                    text.push_str(&format!(".{micro:06}"));
                }
            }
            DatasetCell::Text(text)
        }
        Value::Time(negative, days, hours, minutes, seconds, micro) => {
            let total_hours = u64::from(days) * 24 + u64::from(hours);
            let sign = if negative { "-" } else { "" };
            let mut text = format!("{sign}{total_hours:02}:{minutes:02}:{seconds:02}");
            if micro != 0 {
                text.push_str(&format!(".{micro:06}"));
            }
            DatasetCell::Text(text)
        }
    }
}

fn estimate_row_bytes(row: &[DatasetCell]) -> u64 {
    row.iter()
        .map(|cell| {
            let text = match cell {
                DatasetCell::Text(text) => text.len(),
                _ => 0,
            };
            (std::mem::size_of::<DatasetCell>() + text) as u64
        })
        .sum()
}

/// Leitura em lotes ativa. Enquanto existir, o adapter fica emprestado. Descartá-la antes do
/// fim faz o driver consumir e descartar as linhas restantes do resultado (o MySQL não tem
/// portal com cancelamento barato).
pub struct MysqlBatchStream<'a> {
    result: Option<mysql::QueryResult<'a, 'a, 'a, mysql::Binary>>,
    columns: Vec<(String, String)>,
    types: Vec<mysql::consts::ColumnType>,
    limits: StreamLimits,
    rows_read: u64,
    /// Linha lida que não coube no lote anterior pelo teto de bytes.
    pending: Option<Vec<DatasetCell>>,
    failed: bool,
}

impl MysqlBatchStream<'_> {
    pub fn columns(&self) -> &[(String, String)] {
        &self.columns
    }

    /// Sempre desconhecido: não há `COUNT` prévio.
    pub fn total_rows(&self) -> RowCount {
        RowCount::Unknown
    }

    pub fn rows_read(&self) -> u64 {
        self.rows_read
    }

    /// Próximo lote (no máximo `batch_rows` linhas e `max_inflight_bytes` estimados), ou `None`
    /// no fim. Uma única linha acima do teto falha de forma fechada; nunca é truncada.
    pub fn next_batch(&mut self) -> Result<Option<Batch>, MysqlError> {
        if self.failed {
            return Err(MysqlError::QueryFailed);
        }
        let mut rows: Vec<Vec<DatasetCell>> = Vec::new();
        let mut estimated: u64 = 0;
        while rows.len() < self.limits.batch_rows as usize {
            let row = match self.pending.take() {
                Some(row) => row,
                None => {
                    let Some(result) = self.result.as_mut() else {
                        break;
                    };
                    match result.next() {
                        Some(Ok(row)) => decode_row(row.unwrap(), &self.types),
                        Some(Err(error)) => {
                            self.failed = true;
                            self.result = None;
                            return Err(classify(error));
                        }
                        None => {
                            self.result = None;
                            break;
                        }
                    }
                }
            };
            let bytes = estimate_row_bytes(&row);
            if bytes > self.limits.max_inflight_bytes {
                self.failed = true;
                self.result = None;
                return Err(MysqlError::InflightLimitExceeded);
            }
            if estimated + bytes > self.limits.max_inflight_bytes {
                self.pending = Some(row);
                break;
            }
            estimated += bytes;
            rows.push(row);
        }
        if rows.is_empty() {
            return Ok(None);
        }
        self.rows_read += rows.len() as u64;
        Ok(Some(Batch {
            rows,
            estimated_bytes: estimated,
        }))
    }

    /// Encerra a leitura.
    pub fn finish(self) -> Result<(), MysqlError> {
        if self.failed {
            return Err(MysqlError::QueryFailed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_password_is_used_and_redacted() {
        let profile = MysqlProfile::new("localhost", 3306, "user", "db", "")
            .with_password("test-only-value".to_owned());
        profile.validate().unwrap();
        assert_eq!(profile.resolve_password().unwrap(), "test-only-value");
        assert!(!format!("{profile:?}").contains("test-only-value"));
    }

    #[test]
    fn capabilities_reflect_mysql_not_postgres() {
        let capabilities = MYSQL_CAPABILITIES;
        assert!(!capabilities.transactional_ddl);
        assert!(!capabilities.cancellation);
        assert!(!capabilities.bulk_load);
        assert!(!capabilities.schemas);
        assert!(capabilities.merge_upsert && capabilities.staging && capabilities.stream);
        assert!(capabilities.identifier_quoting && capabilities.parameters);
    }

    #[test]
    fn quotes_with_backticks_and_rejects_invalid() {
        assert_eq!(quote_identifier("a`b").as_deref(), Ok("`a``b`"));
        assert_eq!(quote_identifier(""), Err(MysqlError::InvalidIdentifier));
        assert_eq!(quote_identifier("a\0b"), Err(MysqlError::InvalidIdentifier));
        assert_eq!(
            quote_identifier(&"x".repeat(65)),
            Err(MysqlError::InvalidIdentifier)
        );
        assert!(quote_identifier(&"x".repeat(64)).is_ok());
    }

    #[test]
    fn implicit_commit_statements_are_detected() {
        for sql in [
            "CREATE TABLE t (id int)",
            " drop table t",
            "ALTER TABLE t ADD c int",
            "TRUNCATE t",
            "RENAME TABLE a TO b",
            "START TRANSACTION",
            "BEGIN",
        ] {
            assert!(causes_implicit_commit(sql), "{sql}");
        }
        for sql in [
            "CREATE TEMPORARY TABLE t (id int)",
            "DROP TEMPORARY TABLE t",
            "INSERT INTO t VALUES (1)",
            "SELECT 1",
            "SET @a = 1",
        ] {
            assert!(!causes_implicit_commit(sql), "{sql}");
        }
    }

    #[test]
    fn profile_validation_matches_policy() {
        let mut profile = MysqlProfile::new("db.example.com", 3306, "u", "d", "PW");
        profile.set_tls_mode(TlsMode::Disabled);
        assert_eq!(profile.validate(), Err(MysqlError::InsecureTlsDisabled));
        profile.set_tls_mode(TlsMode::Required);
        assert_eq!(profile.validate(), Ok(()));
        assert!(MysqlProfile::new("h", 0, "u", "d", "PW")
            .validate()
            .is_err());
        assert!(MysqlProfile::new("h", 1, "u", "d", "1BAD")
            .validate()
            .is_err());
        assert!(MysqlProfile::new("127.0.0.1", 1, "u", "d", "PW")
            .validate()
            .is_ok());
    }

    #[test]
    fn cells_decode_without_losing_values() {
        assert_eq!(decode_cell(Value::NULL, None), DatasetCell::Null);
        assert_eq!(
            decode_cell(Value::UInt(u64::MAX), None),
            DatasetCell::Text(u64::MAX.to_string())
        );
        assert_eq!(
            decode_cell(Value::Time(true, 1, 2, 3, 4, 0), None),
            DatasetCell::Text("-26:03:04".to_owned())
        );
        assert_eq!(
            decode_cell(Value::Bytes(vec![0xff, 0x01]), None),
            DatasetCell::Text("ff01".to_owned())
        );
    }

    #[test]
    fn merge_spec_validation() {
        let names = |values: &[&str]| -> Vec<String> {
            values.iter().map(|value| (*value).to_owned()).collect()
        };
        assert!(validate_merge_spec(&names(&["id", "n"]), &names(&["id"])).is_ok());
        assert!(validate_merge_spec(&names(&["n"]), &names(&["id"])).is_err());
        assert!(validate_merge_spec(&names(&["id"]), &[]).is_err());
        assert!(validate_merge_spec(&names(&["id", "id"]), &names(&["id"])).is_err());
    }
}
