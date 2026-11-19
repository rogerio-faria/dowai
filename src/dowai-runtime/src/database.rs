use dowai_core::{DowaiError, Materialization, ProfileTlsMode, WorkspaceProfile};
use dowai_databases_oracle::{
    AdapterError as OracleAdapterError, ConnectionProfile as OracleProfile, OracleAdapter,
    OracleErrorKind, TableIdentifier, TlsMode as OracleTlsMode,
};
use dowai_mysql::{MysqlAdapter, MysqlError, MysqlProfile};
use dowai_postgres::{
    AdapterError, Materialization as PostgresMaterialization, PgProfile, PostgresAdapter,
};
use dowai_sqlserver::{
    AdapterError as SqlServerError, Materialization as SqlServerMaterialization, SqlServerAdapter,
    SqlServerProfile, TlsMode as SqlServerTlsMode,
};
use serde_json::Value;
use std::time::Duration;

#[derive(Clone, Debug)]
pub(crate) enum DatabaseProfile {
    Postgres {
        profile: PgProfile,
        database: String,
    },
    Mysql {
        profile: MysqlProfile,
        database: String,
    },
    Oracle {
        profile: OracleProfile,
        database: String,
    },
    SqlServer {
        profile: SqlServerProfile,
        database: String,
        query_timeout: Option<Duration>,
    },
}

impl DatabaseProfile {
    pub(crate) fn from_workspace(
        profile: WorkspaceProfile,
        configuration: &Value,
    ) -> Result<Self, DowaiError> {
        if profile.tls == ProfileTlsMode::OracleNative && profile.profile_type != "oracle" {
            return Err(DowaiError::Config(
                "Oracle native encryption requires an Oracle profile".to_owned(),
            ));
        }
        match profile.profile_type.as_str() {
            "postgres" => {
                let database = profile.database.clone();
                let adapter_profile = super::postgres_profile(profile, configuration)?;
                Ok(Self::Postgres {
                    profile: adapter_profile,
                    database,
                })
            }
            "mysql" => {
                let database = profile.database.clone();
                let adapter_profile = super::mysql_profile(profile, configuration)?;
                Ok(Self::Mysql {
                    profile: adapter_profile,
                    database,
                })
            }
            "oracle" => {
                let database = profile.database.clone();
                let mut adapter_profile = OracleProfile::new(
                    profile.user,
                    profile.host,
                    profile.port,
                    database.clone(),
                    profile.password_env,
                );
                adapter_profile = adapter_profile.with_tls_mode(match profile.tls {
                    ProfileTlsMode::Required => OracleTlsMode::Required,
                    ProfileTlsMode::Disabled => OracleTlsMode::Disabled,
                    ProfileTlsMode::OracleNative => OracleTlsMode::NativeRequired,
                });
                if let Some(seconds) = configuration["effective"]["database"]["connect_timeout_seconds"].as_u64() {
                    let seconds = u32::try_from(seconds).map_err(|_| {
                        DowaiError::Config(
                            "Effective database.connect_timeout_seconds is invalid".to_owned(),
                        )
                    })?;
                    adapter_profile = adapter_profile.with_connect_timeout_secs(seconds);
                }
                if let Some(seconds) =
                    configuration["effective"]["database"]["query_timeout_seconds"].as_u64()
                {
                    let seconds = u32::try_from(seconds).map_err(|_| {
                        DowaiError::Config(
                            "Effective database.query_timeout_seconds is invalid".to_owned(),
                        )
                    })?;
                    adapter_profile = adapter_profile.with_call_timeout_secs(seconds);
                }
                Ok(Self::Oracle {
                    profile: adapter_profile,
                    database,
                })
            }
            "sqlserver" => {
                let database = profile.database.clone();
                let mut adapter_profile = SqlServerProfile::new(
                    profile.host,
                    profile.port,
                    database.clone(),
                    profile.user,
                    profile.password_env,
                );
                adapter_profile.tls_mode = match profile.tls {
                    ProfileTlsMode::Required => SqlServerTlsMode::Required,
                    ProfileTlsMode::Disabled => SqlServerTlsMode::Disabled,
                    ProfileTlsMode::OracleNative => {
                        return Err(DowaiError::Config(
                            "Oracle native encryption requires an Oracle profile".to_owned(),
                        ));
                    }
                };
                if let Some(seconds) = configuration["effective"]["database"]["connect_timeout_seconds"].as_u64() {
                    adapter_profile.connect_timeout = Duration::from_secs(seconds);
                }
                Ok(Self::SqlServer {
                    profile: adapter_profile,
                    database,
                    query_timeout: configuration["effective"]["database"]["query_timeout_seconds"]
                        .as_u64()
                        .filter(|seconds| *seconds > 0)
                        .map(Duration::from_secs),
                })
            }
            other => Err(DowaiError::Config(format!(
                "SQL execution requires a PostgreSQL, MySQL, Oracle, or SQL Server profile; got '{other}'"
            ))),
        }
    }

    pub(crate) fn quote_identifier(&self, identifier: &str) -> Result<String, DatabaseError> {
        match self {
            Self::Postgres { .. } => {
                dowai_postgres::quote_identifier(identifier).map_err(DatabaseError::Postgres)
            }
            Self::Mysql { .. } => {
                dowai_mysql::quote_identifier(identifier).map_err(DatabaseError::Mysql)
            }
            Self::Oracle { .. } => {
                dowai_databases_oracle::quote_identifier(identifier).map_err(map_oracle_error)
            }
            Self::SqlServer { .. } => {
                dowai_sqlserver::quote_identifier(identifier).map_err(map_sqlserver_error)
            }
        }
    }

    pub(crate) fn relation(&self, schema: &str, table: &str) -> Result<String, DatabaseError> {
        let table = self.quote_identifier(table)?;
        if self.is_mysql() {
            Ok(table)
        } else {
            Ok(format!("{}.{}", self.quote_identifier(schema)?, table))
        }
    }

    pub(crate) fn is_postgres(&self) -> bool {
        matches!(self, Self::Postgres { .. })
    }

    pub(crate) fn is_mysql(&self) -> bool {
        matches!(self, Self::Mysql { .. })
    }

    pub(crate) fn is_native(&self) -> bool {
        matches!(self, Self::Oracle { .. } | Self::SqlServer { .. })
    }

    pub(crate) fn database_name(&self) -> &str {
        match self {
            Self::Postgres { database, .. }
            | Self::Mysql { database, .. }
            | Self::Oracle { database, .. }
            | Self::SqlServer { database, .. } => database,
        }
    }
}

pub(crate) type SqlSources =
    std::collections::BTreeMap<(String, String), std::result::Result<(), &'static str>>;

pub(crate) struct DatabaseAdapter {
    inner: DatabaseConnection,
}

#[derive(Clone, Copy)]
pub(crate) enum NativeTransaction {
    Begin,
    Commit,
}

pub(crate) struct NativePage<'a> {
    pub columns: &'a [String],
    pub order_columns: &'a [String],
    pub offset: u64,
    pub limit: u32,
    pub allow_unkeyed: bool,
}

enum DatabaseConnection {
    Postgres(Box<PostgresAdapter>),
    Mysql(MysqlAdapter),
    Oracle {
        adapter: Option<OracleAdapter>,
        query_timeout: Duration,
    },
    SqlServer {
        runtime: tokio::runtime::Runtime,
        adapter: Option<Box<SqlServerAdapter>>,
        query_timeout: Duration,
    },
}

impl DatabaseAdapter {
    pub(crate) fn connect(profile: &DatabaseProfile) -> Result<Self, DatabaseError> {
        let inner = match profile {
            DatabaseProfile::Postgres { profile, .. } => DatabaseConnection::Postgres(Box::new(
                PostgresAdapter::connect(profile).map_err(DatabaseError::Postgres)?,
            )),
            DatabaseProfile::Mysql { profile, .. } => DatabaseConnection::Mysql(
                MysqlAdapter::connect(profile).map_err(DatabaseError::Mysql)?,
            ),
            DatabaseProfile::Oracle { profile, .. } => {
                let seconds = profile
                    .call_timeout_secs
                    .filter(|seconds| *seconds > 0)
                    .ok_or(DatabaseError::MissingQueryTimeout)?;
                DatabaseConnection::Oracle {
                    adapter: Some(OracleAdapter::connect(profile).map_err(map_oracle_error)?),
                    query_timeout: Duration::from_secs(u64::from(seconds)),
                }
            }
            DatabaseProfile::SqlServer {
                profile,
                query_timeout,
                ..
            } => {
                let query_timeout = query_timeout.ok_or(DatabaseError::MissingQueryTimeout)?;
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| DatabaseError::SqlServer(SqlServerError::ConnectionFailed))?;
                let adapter = runtime
                    .block_on(SqlServerAdapter::connect(profile))
                    .map_err(map_sqlserver_error)?;
                DatabaseConnection::SqlServer {
                    runtime,
                    adapter: Some(Box::new(adapter)),
                    query_timeout,
                }
            }
        };
        Ok(Self { inner })
    }

    pub(crate) fn is_closed(&self) -> bool {
        match &self.inner {
            DatabaseConnection::Postgres(adapter) => adapter.is_closed(),
            DatabaseConnection::Mysql(_) => false,
            DatabaseConnection::Oracle { adapter, .. } => adapter.is_none(),
            DatabaseConnection::SqlServer { adapter, .. } => adapter.is_none(),
        }
    }

    pub(crate) fn test_connection(&mut self) -> Result<(), DatabaseError> {
        match &mut self.inner {
            DatabaseConnection::Postgres(adapter) => {
                adapter.test_connection().map_err(DatabaseError::Postgres)
            }
            DatabaseConnection::Mysql(adapter) => adapter
                .test_connection()
                .map(|_| ())
                .map_err(DatabaseError::Mysql),
            DatabaseConnection::Oracle { adapter, .. } => adapter
                .as_ref()
                .ok_or(DatabaseError::QueryTimedOut)?
                .test_connection()
                .map_err(map_oracle_error),
            DatabaseConnection::SqlServer {
                runtime,
                adapter,
                query_timeout,
            } => {
                let connection = adapter.as_mut().ok_or(DatabaseError::QueryTimedOut)?;
                match runtime.block_on(async {
                    tokio::time::timeout(*query_timeout, connection.test_connection()).await
                }) {
                    Ok(result) => result.map(|_| ()).map_err(map_sqlserver_error),
                    Err(_) => {
                        *adapter = None;
                        Err(DatabaseError::QueryTimedOut)
                    }
                }
            }
        }
    }

    pub(crate) fn native_columns(
        &mut self,
        schema: &str,
        table: &str,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<(String, String)>, DatabaseError> {
        match &mut self.inner {
            DatabaseConnection::Oracle {
                adapter,
                query_timeout,
            } => {
                let connection = adapter.as_ref().ok_or(DatabaseError::QueryTimedOut)?;
                let target = TableIdentifier::in_schema(schema, table);
                let (result, interrupted) =
                    oracle_cancellable(connection, *query_timeout, cancelled, |_| {
                        connection
                            .table_columns(&target)
                            .map(|columns| {
                                columns
                                    .into_iter()
                                    .map(|column| (column.name, column.data_type))
                                    .collect()
                            })
                            .map_err(map_oracle_error)
                    });
                if interrupted {
                    *adapter = None;
                }
                result
            }
            DatabaseConnection::SqlServer {
                runtime,
                adapter,
                query_timeout,
            } => {
                if schema.contains('.') || table.contains('.') {
                    return Err(DatabaseError::SqlServer(SqlServerError::InvalidIdentifier));
                }
                let target = format!("{schema}.{table}");
                let connection = adapter.as_mut().ok_or(DatabaseError::QueryTimedOut)?;
                let result = runtime.block_on(wait_cancellable(
                    connection.inspect_table_columns(&target),
                    *query_timeout,
                    cancelled,
                ));
                if matches!(
                    result,
                    Err(DatabaseError::Cancelled | DatabaseError::QueryTimedOut)
                ) {
                    *adapter = None;
                }
                result?
                    .map(|columns| {
                        columns
                            .into_iter()
                            .map(|column| (column.name, column.data_type))
                            .collect()
                    })
                    .map_err(map_sqlserver_error)
            }
            _ => Err(DatabaseError::UnsupportedDataset),
        }
    }

    pub(crate) fn native_rows(
        &mut self,
        schema: &str,
        table: &str,
        page: NativePage<'_>,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<Vec<Value>>, DatabaseError> {
        let NativePage {
            columns,
            order_columns,
            offset,
            limit,
            allow_unkeyed,
        } = page;
        match &mut self.inner {
            DatabaseConnection::Oracle {
                adapter,
                query_timeout,
            } => {
                let connection = adapter.as_ref().ok_or(DatabaseError::QueryTimedOut)?;
                let (result, interrupted) = oracle_cancellable(
                    connection,
                    *query_timeout,
                    cancelled,
                    |_| {
                        let keys = connection.query_json(
                                "SELECT c.COLUMN_NAME FROM ALL_CONSTRAINTS p JOIN ALL_CONS_COLUMNS c \
                                 ON c.OWNER = p.OWNER AND c.CONSTRAINT_NAME = p.CONSTRAINT_NAME \
                                 WHERE p.OWNER = :1 AND p.TABLE_NAME = :2 AND p.CONSTRAINT_TYPE = 'P' \
                                 ORDER BY c.POSITION",
                                &[&schema, &table],
                            ).map_err(map_oracle_error)?;
                        if keys.is_empty() && !allow_unkeyed {
                            return Err(DatabaseError::DatasetNeedsPrimaryKey);
                        }
                        let key_names = keys
                            .iter()
                            .map(|row| {
                                row["COLUMN_NAME"]
                                    .as_str()
                                    .ok_or(DatabaseError::UnsupportedDataset)
                            })
                            .collect::<Result<Vec<_>, _>>()?;
                        let selected = columns
                            .iter()
                            .map(|name| {
                                dowai_databases_oracle::quote_identifier(name)
                                    .map_err(map_oracle_error)
                            })
                            .collect::<Result<Vec<_>, _>>()?
                            .join(", ");
                        let order = if key_names.is_empty() {
                            let tables = connection.query_json(
                                "SELECT TABLE_NAME FROM ALL_TABLES WHERE OWNER = :1 AND TABLE_NAME = :2",
                                &[&schema, &table],
                            ).map_err(map_oracle_error)?;
                            if tables.is_empty() {
                                return Err(DatabaseError::UnsupportedDataset);
                            }
                            "ROWID".to_owned()
                        } else {
                            key_names
                                .iter()
                                .map(|name| {
                                    dowai_databases_oracle::quote_identifier(name)
                                        .map_err(map_oracle_error)
                                })
                                .collect::<Result<Vec<_>, _>>()?
                                .join(", ")
                        };
                        let relation = format!(
                            "{}.{}",
                            dowai_databases_oracle::quote_identifier(schema)
                                .map_err(map_oracle_error)?,
                            dowai_databases_oracle::quote_identifier(table)
                                .map_err(map_oracle_error)?
                        );
                        let sql = format!("SELECT {selected} FROM {relation} ORDER BY {order} OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY");
                        let rows = connection.query_json(&sql, &[]).map_err(map_oracle_error)?;
                        rows.into_iter()
                            .map(|row| {
                                columns
                                    .iter()
                                    .map(|column| {
                                        row.get(column)
                                            .cloned()
                                            .ok_or(DatabaseError::UnsupportedDataset)
                                    })
                                    .collect()
                            })
                            .collect()
                    },
                );
                if interrupted {
                    *adapter = None;
                }
                result
            }
            DatabaseConnection::SqlServer {
                runtime,
                adapter,
                query_timeout,
            } => {
                let relation = format!(
                    "{}.{}",
                    dowai_sqlserver::quote_identifier(schema).map_err(map_sqlserver_error)?,
                    dowai_sqlserver::quote_identifier(table).map_err(map_sqlserver_error)?
                );
                let selected = columns
                    .iter()
                    .map(|name| {
                        dowai_sqlserver::quote_identifier(name).map_err(map_sqlserver_error)
                    })
                    .collect::<Result<Vec<_>, _>>()?
                    .join(", ");
                let connection = adapter.as_mut().ok_or(DatabaseError::QueryTimedOut)?;
                let result = runtime.block_on(wait_cancellable(async {
                            let keys = connection.query_json_rows(
                                "SELECT COL_NAME(ic.object_id, ic.column_id) AS column_name \
                                 FROM sys.indexes i JOIN sys.index_columns ic \
                                 ON ic.object_id=i.object_id AND ic.index_id=i.index_id \
                                 WHERE i.object_id=OBJECT_ID(@P1) AND i.is_primary_key=1 \
                                 AND ic.key_ordinal > 0 ORDER BY ic.key_ordinal",
                                &[Value::String(relation.clone())],
                            ).await.map_err(map_sqlserver_error)?;
                            let order = if keys.is_empty() {
                                if !allow_unkeyed { return Err(DatabaseError::DatasetNeedsPrimaryKey); }
                                let tables = connection.query_json_rows(
                                    "SELECT 1 AS is_table FROM sys.tables WHERE object_id=OBJECT_ID(@P1)",
                                    &[Value::String(relation.clone())],
                                ).await.map_err(map_sqlserver_error)?;
                                if tables.is_empty() || order_columns.is_empty() { return Err(DatabaseError::UnsupportedDataset); }
                                order_columns.iter().map(|name| {
                                    Ok(format!("CONVERT(varbinary(max), {})", dowai_sqlserver::quote_identifier(name).map_err(map_sqlserver_error)?))
                                }).collect::<Result<Vec<_>, DatabaseError>>()?.join(", ")
                            } else {
                                keys.iter().map(|row| {
                                    let name = row.get("column_name").and_then(Value::as_str).ok_or(DatabaseError::UnsupportedDataset)?;
                                    dowai_sqlserver::quote_identifier(name).map_err(map_sqlserver_error)
                                }).collect::<Result<Vec<_>, _>>()?.join(", ")
                            };
                            let sql = format!("SELECT {selected} FROM {relation} ORDER BY {order} OFFSET {offset} ROWS FETCH NEXT {limit} ROWS ONLY");
                            let rows = connection.query_json_rows(&sql, &[]).await.map_err(map_sqlserver_error)?;
                            rows.into_iter().map(|row| columns.iter().map(|column| row.get(column).cloned().ok_or(DatabaseError::UnsupportedDataset)).collect()).collect()
                        }, *query_timeout, cancelled));
                if matches!(
                    result,
                    Err(DatabaseError::Cancelled | DatabaseError::QueryTimedOut)
                ) {
                    *adapter = None;
                }
                result?
            }
            _ => Err(DatabaseError::UnsupportedDataset),
        }
    }
    fn run_native<T>(
        &mut self,
        cancelled: &(dyn Fn() -> bool + Sync),
        oracle: impl FnOnce(&OracleAdapter) -> Result<T, OracleAdapterError>,
        sqlserver: impl for<'a> FnOnce(
            &'a mut SqlServerAdapter,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<T, SqlServerError>> + 'a>,
        >,
    ) -> Result<T, DatabaseError> {
        if cancelled() {
            return Err(DatabaseError::Cancelled);
        }
        match &mut self.inner {
            DatabaseConnection::Oracle {
                adapter,
                query_timeout,
            } => {
                let connection = adapter.as_ref().ok_or(DatabaseError::QueryTimedOut)?;
                let (result, interrupted) =
                    oracle_cancellable(connection, *query_timeout, cancelled, |_| {
                        oracle(connection).map_err(map_oracle_error)
                    });
                if interrupted {
                    *adapter = None;
                }
                result
            }
            DatabaseConnection::SqlServer {
                runtime,
                adapter,
                query_timeout,
            } => {
                let connection = adapter.as_mut().ok_or(DatabaseError::QueryTimedOut)?;
                let result = runtime
                    .block_on(wait_cancellable(
                        sqlserver(connection),
                        *query_timeout,
                        cancelled,
                    ))
                    .and_then(|result| result.map_err(map_sqlserver_error));
                if matches!(
                    result,
                    Err(DatabaseError::Cancelled | DatabaseError::QueryTimedOut)
                ) {
                    *adapter = None;
                }
                result
            }
            _ => Err(DatabaseError::UnsupportedDataset),
        }
    }

    pub(crate) fn native_transaction(
        &mut self,
        action: NativeTransaction,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), DatabaseError> {
        self.run_native(
            cancelled,
            move |connection| match action {
                NativeTransaction::Begin => connection.begin_transaction(),
                NativeTransaction::Commit => connection.commit(),
            },
            move |connection| {
                Box::pin(async move {
                    match action {
                        NativeTransaction::Begin => connection.begin_transaction().await,
                        NativeTransaction::Commit => connection.commit_transaction().await,
                    }
                })
            },
        )
    }

    pub(crate) fn native_execute(
        &mut self,
        sql: &str,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<u64, DatabaseError> {
        let oracle_sql = sql.to_owned();
        let sqlserver_sql = sql.to_owned();
        self.run_native(
            cancelled,
            move |connection| connection.execute(&oracle_sql, &[]),
            move |connection| Box::pin(async move { connection.execute_sql(&sqlserver_sql).await }),
        )
    }

    pub(crate) fn native_batch(
        &mut self,
        sql: &str,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), DatabaseError> {
        let oracle_sql = sql.to_owned();
        let sqlserver_sql = sql.to_owned();
        self.run_native(
            cancelled,
            move |connection| connection.execute(&oracle_sql, &[]).map(|_| ()),
            move |connection| {
                Box::pin(async move { connection.execute_batch(&sqlserver_sql).await })
            },
        )
    }

    pub(crate) fn native_query(
        &mut self,
        sql: &str,
        parameters: &[String],
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<Vec<Value>, DatabaseError> {
        let oracle_sql = sql.to_owned();
        let sqlserver_sql = sql.to_owned();
        let oracle_parameters = parameters.to_vec();
        let sqlserver_parameters: Vec<Value> =
            parameters.iter().cloned().map(Value::String).collect();
        self.run_native(
            cancelled,
            move |connection| connection.query_json_strings(&oracle_sql, &oracle_parameters),
            move |connection| {
                Box::pin(async move {
                    connection
                        .query_json_rows(&sqlserver_sql, &sqlserver_parameters)
                        .await
                        .map(|rows| {
                            rows.into_iter()
                                .map(|row| Value::Object(row.into_iter().collect()))
                                .collect()
                        })
                })
            },
        )
    }

    pub(crate) fn native_stage_rows(
        &mut self,
        oracle_staging: &str,
        sqlserver_staging: &str,
        rows: &[Value],
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<u64, DatabaseError> {
        let table = TableIdentifier::new(oracle_staging);
        let oracle_rows = rows.to_vec();
        let sqlserver_staging = sqlserver_staging.to_owned();
        let sqlserver_rows = rows
            .iter()
            .map(|row| {
                row.as_object()
                    .map(|object| {
                        object
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    })
                    .ok_or(DatabaseError::UnsupportedDataset)
            })
            .collect::<Result<Vec<dowai_sqlserver::JsonRecord>, _>>()?;
        self.run_native(
            cancelled,
            move |connection| connection.write_rows(&table, &oracle_rows),
            move |connection| {
                Box::pin(async move {
                    connection
                        .append_rows(&sqlserver_staging, &sqlserver_rows)
                        .await
                })
            },
        )
    }

    pub(crate) fn materialize(
        &mut self,
        relation: &str,
        materialization: Materialization,
        query: &str,
        allow_replace: bool,
        full_refresh: bool,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), DatabaseError> {
        if cancelled() {
            return Err(DatabaseError::Cancelled);
        }
        let postgres_materialization = match materialization {
            Materialization::Table => PostgresMaterialization::Table,
            Materialization::View => PostgresMaterialization::View,
        };
        match &mut self.inner {
            DatabaseConnection::Postgres(adapter) if full_refresh => adapter
                .full_refresh_cancellable(relation, postgres_materialization, query, cancelled)
                .map_err(DatabaseError::Postgres),
            DatabaseConnection::Postgres(adapter) => adapter
                .materialize_cancellable(
                    relation,
                    postgres_materialization,
                    query,
                    allow_replace,
                    cancelled,
                )
                .map_err(DatabaseError::Postgres),
            DatabaseConnection::Mysql(adapter) if full_refresh => adapter
                .full_refresh(relation, postgres_materialization, query)
                .map_err(DatabaseError::Mysql),
            DatabaseConnection::Mysql(adapter) => adapter
                .materialize(relation, postgres_materialization, query, allow_replace)
                .map_err(DatabaseError::Mysql),
            DatabaseConnection::Oracle {
                adapter,
                query_timeout,
            } => {
                if full_refresh {
                    return Err(DatabaseError::UnsupportedFullRefresh);
                }
                let target = TableIdentifier::new(relation);
                let quoted_relation =
                    dowai_databases_oracle::quote_identifier(relation).map_err(map_oracle_error)?;
                let connection = adapter.as_ref().ok_or(DatabaseError::QueryTimedOut)?;
                let (result, interrupted) =
                    oracle_cancellable(connection, *query_timeout, cancelled, |interrupted| {
                        match materialization {
                            Materialization::Table => {
                                if allow_replace {
                                    match connection.execute(
                                        &format!("DROP TABLE {quoted_relation} PURGE"),
                                        &[],
                                    ) {
                                        Ok(_)
                                        | Err(OracleAdapterError::Oracle {
                                            kind: OracleErrorKind::ObjectNotFound,
                                            ..
                                        }) => {}
                                        Err(error) => return Err(map_oracle_error(error)),
                                    }
                                }
                                if interrupted.load(std::sync::atomic::Ordering::Acquire) {
                                    return Err(DatabaseError::Cancelled);
                                }
                                connection
                                    .materialize_table(&target, query)
                                    .map_err(map_oracle_error)
                            }
                            Materialization::View => {
                                if allow_replace {
                                    connection
                                        .execute(
                                            &format!(
                                            "CREATE OR REPLACE VIEW {quoted_relation} AS {query}"
                                        ),
                                            &[],
                                        )
                                        .map_err(map_oracle_error)?;
                                    Ok(())
                                } else {
                                    connection
                                        .materialize_view(&target, query)
                                        .map_err(map_oracle_error)
                                }
                            }
                        }
                    });
                if interrupted {
                    *adapter = None;
                }
                result
            }
            DatabaseConnection::SqlServer {
                runtime,
                adapter,
                query_timeout,
            } => {
                if full_refresh {
                    return Err(DatabaseError::UnsupportedFullRefresh);
                }
                let kind = match materialization {
                    Materialization::Table => SqlServerMaterialization::Table,
                    Materialization::View => SqlServerMaterialization::View,
                };
                let connection = adapter.as_mut().ok_or(DatabaseError::QueryTimedOut)?;
                let result = runtime
                    .block_on(wait_cancellable(
                        connection.materialize_unqualified(relation, kind, query, allow_replace),
                        *query_timeout,
                        cancelled,
                    ))
                    .and_then(|result| result.map_err(map_sqlserver_error));
                if matches!(
                    result,
                    Err(DatabaseError::Cancelled | DatabaseError::QueryTimedOut)
                ) {
                    *adapter = None;
                }
                result
            }
        }
    }
}

async fn wait_cancellable<F: std::future::Future>(
    operation: F,
    timeout: Duration,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<F::Output, DatabaseError> {
    let mut operation = std::pin::pin!(operation);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if cancelled() {
            return Err(DatabaseError::Cancelled);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(DatabaseError::QueryTimedOut);
        }
        if let Ok(result) =
            tokio::time::timeout(remaining.min(Duration::from_millis(25)), &mut operation).await
        {
            return Ok(result);
        }
    }
}

fn oracle_cancellable<T>(
    adapter: &OracleAdapter,
    timeout: Duration,
    cancelled: &(dyn Fn() -> bool + Sync),
    operation: impl FnOnce(&std::sync::atomic::AtomicBool) -> Result<T, DatabaseError>,
) -> (Result<T, DatabaseError>, bool) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};

    let interrupted = AtomicBool::new(false);
    let (done, receiver) = mpsc::channel();
    let deadline = std::time::Instant::now() + timeout;
    std::thread::scope(|scope| {
        let interrupted_flag = &interrupted;
        let watchdog = scope.spawn(move || {
            let reason = loop {
                match receiver.recv_timeout(Duration::from_millis(25)) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return None,
                    Err(RecvTimeoutError::Timeout) => {}
                }
                if cancelled() {
                    break DatabaseError::Cancelled;
                }
                if std::time::Instant::now() >= deadline {
                    break DatabaseError::QueryTimedOut;
                }
            };
            interrupted_flag.store(true, Ordering::Release);
            loop {
                if adapter.break_execution().is_err() {
                    return Some(Err(DatabaseError::CancellationFailed));
                }
                match receiver.recv_timeout(Duration::from_millis(25)) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return Some(Ok(reason)),
                    Err(RecvTimeoutError::Timeout) => {}
                }
            }
        });
        let result = operation(&interrupted);
        let _ = done.send(());
        match watchdog.join() {
            Ok(None) => (result, false),
            Ok(Some(Ok(reason))) => (Err(reason), true),
            Ok(Some(Err(error))) => (Err(error), true),
            Err(_) => (Err(DatabaseError::CancellationFailed), true),
        }
    })
}

pub(crate) fn parse_quoted_relation(
    relation: &str,
    opening: char,
    closing: char,
) -> Result<Vec<String>, ()> {
    let mut chars = relation.chars().peekable();
    let mut parts = Vec::new();
    loop {
        if chars.next() != Some(opening) {
            return Err(());
        }
        let mut part = String::new();
        loop {
            match chars.next() {
                Some(character) if character == closing => {
                    if chars.peek() == Some(&closing) {
                        chars.next();
                        part.push(closing);
                    } else {
                        break;
                    }
                }
                Some(character) => part.push(character),
                None => return Err(()),
            }
        }
        parts.push(part);
        match chars.next() {
            None if (1..=2).contains(&parts.len()) => return Ok(parts),
            Some('.') if parts.len() == 1 => {}
            _ => return Err(()),
        }
    }
}

fn map_oracle_error(error: OracleAdapterError) -> DatabaseError {
    match error {
        OracleAdapterError::InvalidProfile(_) => DatabaseError::OracleInvalidProfile,
        OracleAdapterError::InvalidIdentifier(_) => DatabaseError::OracleInvalidIdentifier,
        OracleAdapterError::MissingPasswordEnvironmentVariable(_) => {
            DatabaseError::OracleMissingPassword
        }
        OracleAdapterError::ClientConfigurationUnavailable => {
            DatabaseError::OracleClientConfigurationUnavailable
        }
        OracleAdapterError::Oracle { kind, .. } => DatabaseError::Oracle(kind),
        _ => DatabaseError::Oracle(OracleErrorKind::Other),
    }
}

fn map_sqlserver_error(error: SqlServerError) -> DatabaseError {
    DatabaseError::SqlServer(error)
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum DatabaseError {
    Postgres(AdapterError),
    Mysql(MysqlError),
    Oracle(OracleErrorKind),
    OracleInvalidProfile,
    OracleInvalidIdentifier,
    OracleMissingPassword,
    SqlServer(SqlServerError),
    Cancelled,
    UnsupportedFullRefresh,
    MissingQueryTimeout,
    QueryTimedOut,
    OracleClientConfigurationUnavailable,
    CancellationFailed,
    DatasetNeedsPrimaryKey,
    UnsupportedDataset,
}

impl DatabaseError {
    pub(crate) fn is_transient(self) -> bool {
        match self {
            Self::Postgres(error) => error.is_transient(),
            Self::Mysql(error) => error.is_transient(),
            Self::Oracle(kind) => kind == OracleErrorKind::TransientConnection,
            Self::SqlServer(error) => error.is_transient(),
            Self::OracleInvalidProfile
            | Self::OracleInvalidIdentifier
            | Self::OracleMissingPassword
            | Self::Cancelled
            | Self::UnsupportedFullRefresh
            | Self::MissingQueryTimeout
            | Self::QueryTimedOut
            | Self::OracleClientConfigurationUnavailable
            | Self::CancellationFailed
            | Self::DatasetNeedsPrimaryKey
            | Self::UnsupportedDataset => false,
        }
    }

    pub(crate) fn into_dowai(self) -> DowaiError {
        match self {
            Self::Postgres(error) => match error {
                AdapterError::InvalidProfile(_) => {
                    DowaiError::Config("Invalid PostgreSQL connection profile".to_owned())
                }
                AdapterError::InvalidIdentifier => {
                    DowaiError::Model("Invalid PostgreSQL relation identifier".to_owned())
                }
                AdapterError::DestructiveApprovalRequired => DowaiError::Policy(error.to_string()),
                AdapterError::InsecureTlsDisabled => DowaiError::Policy(
                    "TLS may only be disabled for a local PostgreSQL host".to_owned(),
                ),
                AdapterError::Cancelled => DowaiError::cancelled(),
                AdapterError::MissingPassword
                | AdapterError::TlsConfigurationFailed
                | AdapterError::ConnectionFailed => DowaiError::Connection(error.to_string()),
                _ => DowaiError::Execution(error.to_string()),
            },
            Self::Mysql(error) => match error {
                MysqlError::InvalidProfile(_) => {
                    DowaiError::Config("Invalid MySQL connection profile".to_owned())
                }
                MysqlError::InvalidIdentifier => {
                    DowaiError::Model("Invalid MySQL relation identifier".to_owned())
                }
                MysqlError::InsecureTlsDisabled => {
                    DowaiError::Policy("TLS may only be disabled for a local MySQL host".to_owned())
                }
                MysqlError::DestructiveApprovalRequired => DowaiError::Policy(
                    "Replacing an existing relation requires explicit approval".to_owned(),
                ),
                MysqlError::SchemaMismatch => DowaiError::Execution(error.to_string()),
                MysqlError::MissingPassword
                | MysqlError::TlsConfigurationFailed
                | MysqlError::ConnectionFailed => DowaiError::Connection(error.to_string()),
                _ => DowaiError::Execution(error.to_string()),
            },
            Self::OracleInvalidProfile => {
                DowaiError::Config("Invalid Oracle connection profile".to_owned())
            }
            Self::OracleInvalidIdentifier => {
                DowaiError::Model("Invalid Oracle relation identifier".to_owned())
            }
            Self::OracleMissingPassword => {
                DowaiError::Connection("Oracle password environment variable is not set".to_owned())
            }
            Self::Oracle(kind) => match kind {
                OracleErrorKind::Authentication | OracleErrorKind::Connection => {
                    DowaiError::Connection(format!("Oracle {kind:?} error"))
                }
                _ => DowaiError::Execution(format!("Oracle {kind:?} error")),
            },
            Self::SqlServer(error) => match error {
                SqlServerError::InvalidProfile(_) => {
                    DowaiError::Config("Invalid SQL Server connection profile".to_owned())
                }
                SqlServerError::InvalidIdentifier => {
                    DowaiError::Model("Invalid SQL Server relation identifier".to_owned())
                }
                SqlServerError::TlsDisabledForRemoteHost => DowaiError::Policy(
                    "TLS may only be disabled for a local SQL Server host".to_owned(),
                ),
                SqlServerError::MissingPassword | SqlServerError::InvalidPasswordEnvironment => {
                    DowaiError::Connection(
                        "SQL Server password environment variable is not set".to_owned(),
                    )
                }
                SqlServerError::ConnectionFailed
                | SqlServerError::Timeout
                | SqlServerError::LoginTimeout => DowaiError::Connection(error.to_string()),
                _ => DowaiError::Execution(error.to_string()),
            },
            Self::Cancelled => DowaiError::cancelled(),
            Self::UnsupportedFullRefresh => DowaiError::Execution(
                "Atomic full refresh is not supported by this database adapter".to_owned(),
            ),
            Self::MissingQueryTimeout => {
                DowaiError::Config("A positive database query timeout is required".to_owned())
            }
            Self::QueryTimedOut => DowaiError::Execution(
                "Database query timed out; the connection was discarded".to_owned(),
            ),
            Self::OracleClientConfigurationUnavailable => DowaiError::Config(
                "Oracle client configuration cannot guarantee the requested encryption mode"
                    .to_owned(),
            ),
            Self::CancellationFailed => DowaiError::Execution(
                "Could not interrupt the database operation; its outcome may be unknown".to_owned(),
            ),
            Self::DatasetNeedsPrimaryKey => DowaiError::Execution(
                "A primary key is required for ordered dataset reads".to_owned(),
            ),
            Self::UnsupportedDataset => DowaiError::Execution(
                "Dataset is not supported by this database adapter".to_owned(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn profile(kind: &str) -> WorkspaceProfile {
        serde_json::from_value(json!({
            "type": kind,
            "host": "localhost",
            "port": 1234,
            "user": "user",
            "database": "service",
            "password_env": "DOWAI_TEST_PASSWORD",
            "tls": "required"
        }))
        .unwrap()
    }

    #[test]
    fn native_profiles_preserve_tls_and_database() {
        let configuration = json!({"effective": {"database": {
            "connect_timeout_seconds": 17,
            "query_timeout_seconds": 23
        }}});
        let oracle = DatabaseProfile::from_workspace(profile("oracle"), &configuration).unwrap();
        let sqlserver =
            DatabaseProfile::from_workspace(profile("sqlserver"), &configuration).unwrap();
        match oracle {
            DatabaseProfile::Oracle { profile, database } => {
                assert_eq!(database, "service");
                assert_eq!(profile.tls_mode, OracleTlsMode::Required);
                assert_eq!(profile.connect_timeout_secs, Some(17));
                assert_eq!(profile.call_timeout_secs, Some(23));
            }
            _ => panic!("expected Oracle profile"),
        }
        match sqlserver {
            DatabaseProfile::SqlServer {
                profile,
                database,
                query_timeout,
            } => {
                assert_eq!(database, "service");
                assert_eq!(profile.tls_mode, SqlServerTlsMode::Required);
                assert_eq!(profile.connect_timeout, Duration::from_secs(17));
                assert_eq!(query_timeout, Some(Duration::from_secs(23)));
            }
            _ => panic!("expected SQL Server profile"),
        }
        let mut workspace = profile("oracle");
        workspace.tls = ProfileTlsMode::OracleNative;
        match DatabaseProfile::from_workspace(workspace, &configuration).unwrap() {
            DatabaseProfile::Oracle { profile, .. } => {
                assert_eq!(profile.tls_mode, OracleTlsMode::NativeRequired);
            }
            _ => panic!("expected Oracle profile"),
        }
    }

    #[test]
    fn quoted_relation_parser_preserves_escaped_delimiters() {
        assert_eq!(
            parse_quoted_relation("\"a\"\"b\".\"c.d\"", '"', '"'),
            Ok(vec!["a\"b".to_owned(), "c.d".to_owned()])
        );
        assert_eq!(
            parse_quoted_relation("[a]]b].[c]", '[', ']'),
            Ok(vec!["a]b".to_owned(), "c".to_owned()])
        );
        assert!(parse_quoted_relation("[a].[b].[c]", '[', ']').is_err());
        assert!(parse_quoted_relation("[a", '[', ']').is_err());
    }

    #[test]
    fn native_adapters_require_query_timeout_before_connecting() {
        for kind in ["oracle", "sqlserver"] {
            let profile = DatabaseProfile::from_workspace(profile(kind), &json!({})).unwrap();
            assert!(matches!(
                DatabaseAdapter::connect(&profile),
                Err(DatabaseError::MissingQueryTimeout)
            ));
        }
    }

    #[test]
    #[ignore = "requires an encrypted SQL Server test connection"]
    fn sqlserver_runtime_connection_probe() {
        let workspace: WorkspaceProfile = serde_json::from_value(json!({
            "type": "sqlserver",
            "host": std::env::var("DOWAI_TEST_SQLSERVER_HOST").expect("SQL Server host is required"),
            "port": std::env::var("DOWAI_TEST_SQLSERVER_PORT").expect("SQL Server port is required").parse::<u16>().expect("invalid SQL Server port"),
            "user": std::env::var("DOWAI_TEST_SQLSERVER_USER").expect("SQL Server user is required"),
            "database": std::env::var("DOWAI_TEST_SQLSERVER_DATABASE").expect("SQL Server database is required"),
            "password_env": "DOWAI_SQLSERVER_PASSWORD",
            "tls": "required"
        })).unwrap();
        let configuration = json!({"effective": {"database": {
            "connect_timeout_seconds": 30,
            "query_timeout_seconds": 30
        }}});
        let profile = DatabaseProfile::from_workspace(workspace, &configuration).unwrap();
        let mut adapter = DatabaseAdapter::connect(&profile)
            .unwrap_or_else(|error| panic!("SQL Server connection failed: {:?}", error));
        assert!(adapter.test_connection().is_ok(), "connection probe failed");
        assert!(matches!(
            adapter.materialize(
                "[dbo].[dowai_unused]",
                Materialization::Table,
                "SELECT 1 AS id",
                false,
                true,
                &|| false
            ),
            Err(DatabaseError::UnsupportedFullRefresh)
        ));
        let name = format!("dowai_test_{}", uuid::Uuid::new_v4().simple());
        let relation = format!("[{name}]");
        let create = adapter.materialize(
            &name,
            Materialization::Table,
            "SELECT 7 AS id",
            false,
            false,
            &|| false,
        );
        let replace = adapter.materialize(
            &name,
            Materialization::Table,
            "SELECT missing_column",
            true,
            false,
            &|| false,
        );
        drop(adapter);
        let mut check = DatabaseAdapter::connect(&profile).unwrap_or_else(|error| {
            panic!("SQL Server verification connection failed: {:?}", error)
        });
        let DatabaseConnection::SqlServer {
            runtime, adapter, ..
        } = &mut check.inner
        else {
            panic!("expected SQL Server connection");
        };
        let connection = adapter.as_mut().expect("SQL Server connection missing");
        let rows = runtime
            .block_on(connection.query_json_rows(&format!("SELECT [id] FROM {relation}"), &[]));
        let ids = vec!["id".to_owned()];
        let unkeyed = check.native_rows(
            "dbo",
            &name,
            NativePage {
                columns: &ids,
                order_columns: &ids,
                offset: 0,
                limit: 10,
                allow_unkeyed: true,
            },
            &|| false,
        );
        let DatabaseConnection::SqlServer {
            runtime,
            adapter: Some(connection),
            ..
        } = &mut check.inner
        else {
            panic!("SQL Server connection missing");
        };
        let add_key = runtime.block_on(
            connection.execute_sql(&format!("ALTER TABLE {relation} ALTER COLUMN [id] INT NOT NULL; ALTER TABLE {relation} ADD PRIMARY KEY ([id])")),
        );
        let columns = check.native_columns("dbo", &name, &|| false);
        let page = || NativePage {
            columns: &ids,
            order_columns: &ids,
            offset: 0,
            limit: 10,
            allow_unkeyed: false,
        };
        let batch = check.native_rows("dbo", &name, page(), &|| false);
        let stage_test = match &mut check.inner {
            DatabaseConnection::SqlServer {
                runtime,
                adapter: Some(connection),
                ..
            } => runtime.block_on(async {
                connection.begin_transaction().await?;
                let result = async {
                    connection
                        .execute_batch(&format!(
                            "SELECT TOP (0) * INTO [#dowai_stage] FROM {relation}"
                        ))
                        .await?;
                    connection
                        .execute_sql("INSERT INTO [#dowai_stage] ([id]) VALUES (9)")
                        .await?;
                    connection
                        .execute_sql(&format!(
                            "INSERT INTO {relation} SELECT * FROM [#dowai_stage]"
                        ))
                        .await?;
                    Ok::<(), SqlServerError>(())
                }
                .await;
                let rollback = connection.rollback_transaction().await;
                result.and(rollback)
            }),
            _ => panic!("SQL Server connection missing"),
        };
        let after_rollback = check.native_rows("dbo", &name, page(), &|| false);
        let cleanup = match &mut check.inner {
            DatabaseConnection::SqlServer {
                runtime,
                adapter: Some(connection),
                ..
            } => runtime
                .block_on(connection.execute_sql(&format!("DROP TABLE IF EXISTS {relation}"))),
            _ => panic!("SQL Server connection missing"),
        };
        assert!(cleanup.is_ok(), "SQL Server test cleanup failed");
        assert!(
            create.is_ok(),
            "SQL Server materialization failed: {:?}",
            create
        );
        assert!(
            replace.is_err(),
            "invalid SQL Server replacement unexpectedly succeeded"
        );
        assert_eq!(rows.unwrap()[0].get("id"), Some(&json!("7")));
        assert_eq!(unkeyed.unwrap(), vec![vec![json!("7")]]);
        assert!(add_key.is_ok(), "SQL Server test key creation failed");
        assert_eq!(columns.unwrap()[0].0, "id");
        assert_eq!(batch.unwrap(), vec![vec![json!("7")]]);
        assert!(
            stage_test.is_ok(),
            "SQL Server temporary staging failed: {:?}",
            stage_test
        );
        assert_eq!(after_rollback.unwrap(), vec![vec![json!("7")]]);
        let DatabaseConnection::SqlServer {
            runtime,
            adapter: Some(connection),
            ..
        } = &mut check.inner
        else {
            panic!("SQL Server connection missing");
        };
        let started = std::time::Instant::now();
        let outcome = runtime.block_on(wait_cancellable(
            connection.execute_batch("WAITFOR DELAY '00:00:05'"),
            Duration::from_secs(3),
            &|| started.elapsed() >= Duration::from_millis(200),
        ));
        assert!(matches!(outcome, Err(DatabaseError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[test]
    #[ignore = "requires Oracle Client, native encryption and a test connection"]
    fn oracle_runtime_connection_probe() {
        let workspace: WorkspaceProfile = serde_json::from_value(json!({
            "type": "oracle",
            "host": std::env::var("DOWAI_TEST_ORACLE_HOST").expect("Oracle host is required"),
            "port": std::env::var("DOWAI_TEST_ORACLE_PORT").expect("Oracle port is required").parse::<u16>().expect("invalid Oracle port"),
            "user": std::env::var("DOWAI_TEST_ORACLE_USER").expect("Oracle user is required"),
            "database": std::env::var("DOWAI_TEST_ORACLE_SERVICE").expect("Oracle service is required"),
            "password_env": "DOWAI_ORACLE_PASSWORD",
            "tls": "oracle_native"
        })).unwrap();
        let configuration = json!({"effective": {"database": {
            "connect_timeout_seconds": 30,
            "query_timeout_seconds": 30
        }}});
        let profile = DatabaseProfile::from_workspace(workspace, &configuration).unwrap();
        let mut adapter = DatabaseAdapter::connect(&profile)
            .unwrap_or_else(|error| panic!("Oracle connection failed: {:?}", error));
        assert!(adapter.test_connection().is_ok(), "connection probe failed");
        assert!(matches!(
            adapter.materialize(
                "\"DOWAI_UNUSED\"",
                Materialization::Table,
                "SELECT 1 AS id FROM DUAL",
                false,
                true,
                &|| false
            ),
            Err(DatabaseError::UnsupportedFullRefresh)
        ));
        let name = format!(
            "DOWAI_TEST_{}",
            &uuid::Uuid::new_v4().simple().to_string()[..16]
        );
        let relation = format!("\"{name}\"");
        let create = adapter.materialize(
            &name,
            Materialization::Table,
            "SELECT 7 AS ID FROM DUAL",
            false,
            false,
            &|| false,
        );
        let DatabaseConnection::Oracle {
            adapter: Some(connection),
            ..
        } = &adapter.inner
        else {
            panic!("expected Oracle connection");
        };
        let rows = connection.query_json(&format!("SELECT ID FROM {relation}"), &[]);
        let schema = connection
            .query_json(
                "SELECT SYS_CONTEXT('USERENV','CURRENT_SCHEMA') AS SCHEMA_NAME FROM DUAL",
                &[],
            )
            .unwrap()[0]["SCHEMA_NAME"]
            .as_str()
            .unwrap()
            .to_owned();
        let ids = vec!["ID".to_owned()];
        let unkeyed = adapter.native_rows(
            &schema,
            &name,
            NativePage {
                columns: &ids,
                order_columns: &ids,
                offset: 0,
                limit: 10,
                allow_unkeyed: true,
            },
            &|| false,
        );
        let DatabaseConnection::Oracle {
            adapter: Some(connection),
            ..
        } = &adapter.inner
        else {
            panic!("Oracle connection missing");
        };
        let add_key =
            connection.execute(&format!("ALTER TABLE {relation} ADD PRIMARY KEY (ID)"), &[]);
        let staging = "ORA$PTT_DOWAI_STAGE";
        let private_stage = connection.execute(
            &format!("CREATE PRIVATE TEMPORARY TABLE {staging} ON COMMIT DROP DEFINITION AS SELECT * FROM {relation} WHERE 1=0"),
            &[],
        );
        let stage_rollback = if private_stage.is_ok() {
            connection
                .execute(&format!("INSERT INTO {staging} (ID) VALUES (9)"), &[])
                .and_then(|_| {
                    connection.execute(
                        &format!("INSERT INTO {relation} SELECT * FROM {staging}"),
                        &[],
                    )
                })
                .and_then(|_| connection.rollback())
        } else {
            connection.rollback()
        };
        let after_rollback =
            connection.query_json(&format!("SELECT COUNT(*) AS C FROM {relation}"), &[]);
        let columns = adapter.native_columns(&schema, &name, &|| false);
        let batch = adapter.native_rows(
            &schema,
            &name,
            NativePage {
                columns: &ids,
                order_columns: &ids,
                offset: 0,
                limit: 10,
                allow_unkeyed: false,
            },
            &|| false,
        );
        let cleanup = match &adapter.inner {
            DatabaseConnection::Oracle {
                adapter: Some(connection),
                ..
            } => connection.execute(&format!("DROP TABLE {relation} PURGE"), &[]),
            _ => panic!("Oracle connection missing"),
        };
        assert!(cleanup.is_ok(), "Oracle test cleanup failed");
        assert!(
            create.is_ok(),
            "Oracle materialization failed: {:?}",
            create
        );
        assert_eq!(rows.unwrap()[0]["ID"], json!("7"));
        assert_eq!(unkeyed.unwrap(), vec![vec![json!("7")]]);
        assert!(add_key.is_ok(), "Oracle test key creation failed");
        assert!(
            private_stage.is_ok(),
            "Oracle private staging table creation failed: {:?}",
            private_stage
        );
        assert!(stage_rollback.is_ok(), "Oracle staging rollback failed");
        assert_eq!(after_rollback.unwrap()[0]["C"], json!("1"));
        assert_eq!(columns.unwrap()[0].0, "ID");
        assert_eq!(batch.unwrap(), vec![vec![json!("7")]]);
        let DatabaseConnection::Oracle {
            adapter: Some(connection),
            ..
        } = &adapter.inner
        else {
            panic!("Oracle connection missing");
        };
        let started = std::time::Instant::now();
        let (outcome, interrupted) = oracle_cancellable(
            connection,
            Duration::from_secs(3),
            &|| started.elapsed() >= Duration::from_millis(200),
            |_| {
                connection.query_json(
                "SELECT COUNT(*) AS C FROM ALL_OBJECTS a CROSS JOIN ALL_OBJECTS b CROSS JOIN ALL_OBJECTS c",
                &[],
            ).map(|_| ()).map_err(map_oracle_error)
            },
        );
        assert!(interrupted);
        assert!(matches!(outcome, Err(DatabaseError::Cancelled)));
        assert!(started.elapsed() < Duration::from_secs(3));
    }
}
