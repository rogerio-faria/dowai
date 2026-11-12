//! Contrato de adapter SQL (plano §10.1, ENT-029).
//!
//! Cada SGBD declara [`Capabilities`] verdadeiras. Operações ainda não implementadas por um
//! adapter ficam com a capability `false`; o motor não deve depender delas sem checar a flag.
//! `stream_query`, `write_batch` e `merge` serão acrescentados por ENT-031 e ENT-032.

use postgres::types::ToSql;

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
}

/// Capabilities reais do adapter PostgreSQL. Só declara o que os testes de contrato verificam.
pub const POSTGRES_CAPABILITIES: Capabilities = Capabilities {
    transactional_ddl: true,
    identifier_quoting: true,
    schemas: false,
    staging: false,
    bulk_load: false,
    stream: false,
    merge_upsert: false,
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
        // Ainda não implementadas (ENT-031, ENT-032 e staging/schemas fora da fatia).
        assert!(!capabilities.stream);
        assert!(!capabilities.merge_upsert);
        assert!(!capabilities.staging);
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
