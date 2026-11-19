//! Contrato de adapter SQL (plano §10.1, ENT-029).
//!
//! Cada SGBD declara [`Capabilities`] verdadeiras. Operações ainda não implementadas por um
//! adapter ficam com a capability `false`; o motor não deve depender delas sem checar a flag.
//! Escrita em staging e `merge` (ENT-032) estão em [`SqlAdapter::merge`].

use postgres::types::ToSql;

use crate::stream::{BatchStream, StreamLimits};
use crate::write::MergeCounters;
use crate::{classify_failure, quote_identifier, AdapterError, PostgresAdapter};

/// Capacidades declaradas por um adapter. Todas `false` significa "não suportado".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capabilities {
    /// DDL (criar/substituir/remover relações) participa de transação e faz rollback.
    pub transactional_ddl: bool,
    /// Identificadores são escapados pelo dialeto (`quote_identifier`).
    pub identifier_quoting: bool,
    /// Relações qualificadas por schema.
    pub schemas: bool,
    /// Carga em tabela de staging antes do destino.
    pub staging: bool,
    /// Carga em massa nativa (COPY ou equivalente).
    pub bulk_load: bool,
    /// Leitura em streaming com backpressure (ENT-031).
    pub stream: bool,
    /// Merge/upsert nativo (ENT-032).
    pub merge_upsert: bool,
    /// Conexão com TLS disponível no driver.
    pub tls: bool,
    /// Valores passados como parâmetros SQL, nunca interpolados.
    pub parameters: bool,
    /// Contagem de linhas afetadas retornada pelas operações de escrita.
    pub row_count: bool,
    /// Cancelamento de operação em andamento.
    pub cancellation: bool,
}

/// Coluna de uma relação introspeccionada: nome e tipo nativo do SGBD.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
}

/// Interface comum dos adapters SQL.
///
/// Transações são controladas explicitamente: `begin_transaction`, depois `commit` ou
/// `rollback`. Iniciar transação já aberta e confirmar/desfazer sem transação aberta retornam
/// [`AdapterError::InvalidTransactionState`].
pub trait SqlAdapter {
    fn capabilities(&self) -> Capabilities;
    /// Confirma a conexão executando consulta trivial e devolve as capabilities do adapter.
    fn test_connection(&mut self) -> Result<Capabilities, AdapterError>;
    /// Colunas e tipos de `table` (identificador simples, sem schema).
    fn introspect_table(&mut self, table: &str) -> Result<Vec<ColumnInfo>, AdapterError>;
    /// Executa uma instrução com parâmetros vinculados; devolve linhas afetadas.
    fn execute_sql(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, AdapterError>;
    fn begin_transaction(&mut self) -> Result<(), AdapterError>;
    fn commit(&mut self) -> Result<(), AdapterError>;
    fn rollback(&mut self) -> Result<(), AdapterError>;
    /// Leitura em lotes com backpressure: nada além do lote pedido é buscado (ENT-031).
    fn stream_query<'a>(
        &'a mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        limits: StreamLimits,
    ) -> Result<BatchStream<'a>, AdapterError>;
    /// Cria a staging temporária do destino (mesmas colunas). Exige transação aberta (ENT-032).
    fn create_staging(&mut self, target: &str, staging: &str) -> Result<(), AdapterError>;
    /// Insere um lote na staging, uma linha por entrada, valores na ordem de `columns`.
    /// Exige transação aberta e devolve linhas inseridas (ENT-032).
    fn write_staging_batch(
        &mut self,
        staging: &str,
        columns: &[String],
        rows: &[Vec<&(dyn ToSql + Sync)>],
    ) -> Result<u64, AdapterError>;
    /// Upsert da staging no destino pelas chaves; contagens de inseridos e atualizados.
    /// Exige transação aberta (ENT-032).
    fn merge(
        &mut self,
        staging: &str,
        target: &str,
        columns: &[String],
        keys: &[String],
    ) -> Result<MergeCounters, AdapterError>;
}

/// Capabilities reais do adapter PostgreSQL. Só declara o que os testes de contrato verificam.
pub const POSTGRES_CAPABILITIES: Capabilities = Capabilities {
    transactional_ddl: true,
    identifier_quoting: true,
    schemas: false,
    staging: true,
    bulk_load: false,
    stream: true,
    merge_upsert: true,
    tls: true,
    parameters: true,
    row_count: true,
    cancellation: true,
};

impl SqlAdapter for PostgresAdapter {
    fn capabilities(&self) -> Capabilities {
        POSTGRES_CAPABILITIES
    }

    fn test_connection(&mut self) -> Result<Capabilities, AdapterError> {
        PostgresAdapter::test_connection(self)?;
        Ok(POSTGRES_CAPABILITIES)
    }

    fn introspect_table(&mut self, table: &str) -> Result<Vec<ColumnInfo>, AdapterError> {
        let quoted = quote_identifier(table)?;
        Ok(self
            .describe_relation(&quoted)?
            .into_iter()
            .map(|(name, data_type)| ColumnInfo { name, data_type })
            .collect())
    }

    fn execute_sql(
        &mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
    ) -> Result<u64, AdapterError> {
        PostgresAdapter::execute(self, sql, params)
    }

    fn begin_transaction(&mut self) -> Result<(), AdapterError> {
        if self.in_transaction {
            return Err(AdapterError::InvalidTransactionState);
        }
        self.client
            .batch_execute("BEGIN")
            .map_err(classify_failure)?;
        self.in_transaction = true;
        Ok(())
    }

    fn commit(&mut self) -> Result<(), AdapterError> {
        if !self.in_transaction {
            return Err(AdapterError::InvalidTransactionState);
        }
        self.client
            .batch_execute("COMMIT")
            .map_err(classify_failure)?;
        self.in_transaction = false;
        Ok(())
    }

    fn rollback(&mut self) -> Result<(), AdapterError> {
        if !self.in_transaction {
            return Err(AdapterError::InvalidTransactionState);
        }
        self.client
            .batch_execute("ROLLBACK")
            .map_err(classify_failure)?;
        self.in_transaction = false;
        Ok(())
    }

    fn stream_query<'a>(
        &'a mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        limits: StreamLimits,
    ) -> Result<BatchStream<'a>, AdapterError> {
        PostgresAdapter::stream_query(self, sql, params, limits)
    }

    fn create_staging(&mut self, target: &str, staging: &str) -> Result<(), AdapterError> {
        PostgresAdapter::create_staging(self, target, staging)
    }

    fn write_staging_batch(
        &mut self,
        staging: &str,
        columns: &[String],
        rows: &[Vec<&(dyn ToSql + Sync)>],
    ) -> Result<u64, AdapterError> {
        PostgresAdapter::write_staging_batch(self, staging, columns, rows)
    }

    fn merge(
        &mut self,
        staging: &str,
        target: &str,
        columns: &[String],
        keys: &[String],
    ) -> Result<MergeCounters, AdapterError> {
        PostgresAdapter::merge_staging(self, staging, target, columns, keys)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_declares_only_verified_capabilities() {
        let capabilities = POSTGRES_CAPABILITIES;
        assert!(capabilities.transactional_ddl);
        assert!(capabilities.identifier_quoting);
        assert!(capabilities.parameters);
        assert!(capabilities.row_count);
        assert!(capabilities.tls);
        assert!(capabilities.cancellation);
        assert!(capabilities.stream);
        assert!(capabilities.merge_upsert);
        assert!(capabilities.staging);
        // Ainda não implementadas (carga em massa e schemas fora da fatia).
        assert!(!capabilities.bulk_load);
        assert!(!capabilities.schemas);
    }

    #[test]
    fn default_capabilities_declare_nothing() {
        let capabilities = Capabilities::default();
        assert!(!capabilities.transactional_ddl);
        assert!(!capabilities.stream);
        assert!(!capabilities.merge_upsert);
        assert!(!capabilities.tls);
    }

    #[test]
    fn trait_is_object_safe() {
        fn accepts(_adapter: &mut dyn SqlAdapter) {}
        let _ = accepts as fn(&mut dyn SqlAdapter);
    }
}
