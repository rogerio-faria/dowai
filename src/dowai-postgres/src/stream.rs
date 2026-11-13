//! Leitura em lotes com backpressure (ENT-031, plano §10.1 `stream_query`).
//!
//! A consulta roda numa transação somente leitura com portal nomeado. Cada chamada a
//! [`BatchStream::next_batch`] busca no máximo `batch_rows` linhas do servidor; nada além do
//! lote pedido é lido antes que o consumidor peça o próximo (backpressure por pull).
//!
//! O tamanho do lote também respeita `max_inflight_bytes`, com uma estimativa conservadora da
//! memória ocupada pelas células. Um lote que excederia o teto falha de forma fechada
//! ([`AdapterError::InflightLimitExceeded`]); nunca é truncado em silêncio. A primeira busca é de
//! uma linha, para medir o tamanho real antes de dimensionar os lotes seguintes.
//!
//! Sem contagem prévia: a leitura não executa `COUNT`, então [`BatchStream::total_rows`] devolve
//! [`RowCount::Unknown`] até o fim. Nenhum total é inventado.

use std::mem::size_of;

use postgres::types::ToSql;
use postgres::{Portal, Transaction};

use crate::{
    classify_failure, decode_cell, ensure_single_statement, AdapterError, DatasetCell,
    PostgresAdapter,
};

/// Limite superior de linhas por lote (igual ao teto de `database.fetch_batch_rows`).
pub const STREAM_BATCH_ROWS_MAX: u32 = 1_000_000;

/// Limites de uma leitura em lotes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamLimits {
    /// Linhas máximas por lote (`database.fetch_batch_rows`).
    pub batch_rows: u32,
    /// Teto de bytes estimados mantidos em voo, isto é, no lote entregue ao consumidor.
    pub max_inflight_bytes: u64,
}

/// Contagem total de linhas de uma leitura.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowCount {
    Known(u64),
    Unknown,
}

/// Lote de linhas já decodificadas e sua estimativa de memória.
#[derive(Debug, PartialEq)]
pub struct Batch {
    pub rows: Vec<Vec<DatasetCell>>,
    pub estimated_bytes: u64,
}

/// Leitura em lotes ativa. Enquanto existir, o adapter fica emprestado; a transação somente
/// leitura é revertida ao ser descartada sem término explícito.
pub struct BatchStream<'a> {
    transaction: Option<Transaction<'a>>,
    portal: Portal,
    columns: Vec<(String, String)>,
    limits: StreamLimits,
    largest_row_bytes: u64,
    rows_read: u64,
    state: State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Open,
    Finished,
    Failed,
}

impl PostgresAdapter {
    /// Inicia leitura em lotes de `sql` com `params` vinculados. Recusa uso dentro de transação
    /// já aberta por `begin_transaction`.
    pub fn stream_query<'a>(
        &'a mut self,
        sql: &str,
        params: &[&(dyn ToSql + Sync)],
        limits: StreamLimits,
    ) -> Result<BatchStream<'a>, AdapterError> {
        validate_limits(limits)?;
        if self.in_transaction {
            return Err(AdapterError::InvalidTransactionState);
        }
        ensure_single_statement(sql)?;
        let mut transaction = self
            .client
            .build_transaction()
            .read_only(true)
            .start()
            .map_err(classify_failure)?;
        let statement = transaction.prepare(sql).map_err(classify_failure)?;
        let columns: Vec<(String, String)> = statement
            .columns()
            .iter()
            .map(|column| (column.name().to_owned(), column.type_().name().to_owned()))
            .collect();
        if columns.is_empty() {
            return Err(AdapterError::QueryFailed);
        }
        let portal = transaction
            .bind(&statement, params)
            .map_err(classify_failure)?;
        Ok(BatchStream {
            transaction: Some(transaction),
            portal,
            columns,
            limits,
            largest_row_bytes: 0,
            rows_read: 0,
            state: State::Open,
        })
    }
}

impl BatchStream<'_> {
    /// Colunas (nome, tipo nativo) na ordem do lote.
    pub fn columns(&self) -> &[(String, String)] {
        &self.columns
    }

    /// Total de linhas: sempre desconhecido durante a leitura (sem `COUNT` prévio).
    pub fn total_rows(&self) -> RowCount {
        RowCount::Unknown
    }

    /// Linhas já buscadas do servidor e entregues ao consumidor.
    pub fn rows_read(&self) -> u64 {
        self.rows_read
    }

    /// Devolve o próximo lote, ou `None` quando não há mais linhas. Após erro, a leitura fica
    /// inutilizável e a transação é revertida.
    pub fn next_batch(&mut self) -> Result<Option<Batch>, AdapterError> {
        match self.state {
            State::Finished => return Ok(None),
            State::Failed => return Err(AdapterError::QueryFailed),
            State::Open => {}
        }
        match self.fetch() {
            Ok(batch) => Ok(batch),
            Err(error) => {
                self.state = State::Failed;
                self.transaction = None;
                Err(error)
            }
        }
    }

    /// Encerra a leitura e confirma a transação somente leitura.
    pub fn finish(mut self) -> Result<(), AdapterError> {
        if let Some(transaction) = self.transaction.take() {
            transaction.commit().map_err(classify_failure)?;
        }
        self.state = State::Finished;
        Ok(())
    }

    fn fetch(&mut self) -> Result<Option<Batch>, AdapterError> {
        let fetch_rows = next_fetch_size(
            self.limits.batch_rows,
            self.limits.max_inflight_bytes,
            self.largest_row_bytes,
        );
        let transaction = self
            .transaction
            .as_mut()
            .ok_or(AdapterError::InvalidTransactionState)?;
        let rows = transaction
            .query_portal(&self.portal, i32::try_from(fetch_rows).unwrap_or(i32::MAX))
            .map_err(classify_failure)?;
        if rows.is_empty() {
            self.state = State::Finished;
            if let Some(transaction) = self.transaction.take() {
                transaction.commit().map_err(classify_failure)?;
            }
            return Ok(None);
        }

        let mut decoded = Vec::with_capacity(rows.len());
        let mut batch_bytes = 0u64;
        for row in &rows {
            let mut cells = Vec::with_capacity(self.columns.len());
            let mut row_bytes = 0u64;
            for (index, (_, data_type)) in self.columns.iter().enumerate() {
                let cell = decode_cell(row, index, data_type)?;
                row_bytes = row_bytes.saturating_add(cell_bytes(&cell));
                cells.push(cell);
            }
            batch_bytes = batch_bytes.saturating_add(row_bytes);
            self.largest_row_bytes = self.largest_row_bytes.max(row_bytes);
            decoded.push(cells);
        }
        if batch_bytes > self.limits.max_inflight_bytes {
            return Err(AdapterError::InflightLimitExceeded);
        }
        self.rows_read = self.rows_read.saturating_add(decoded.len() as u64);
        Ok(Some(Batch {
            rows: decoded,
            estimated_bytes: batch_bytes,
        }))
    }
}

fn validate_limits(limits: StreamLimits) -> Result<(), AdapterError> {
    if limits.batch_rows == 0
        || limits.batch_rows > STREAM_BATCH_ROWS_MAX
        || limits.max_inflight_bytes == 0
    {
        return Err(AdapterError::InvalidStreamLimit);
    }
    Ok(())
}

/// Linhas a buscar na próxima chamada. Sem medida anterior, busca uma linha; depois, o menor
/// entre `batch_rows` e quantas linhas da maior medida cabem em `max_inflight_bytes`.
fn next_fetch_size(batch_rows: u32, max_inflight_bytes: u64, largest_row_bytes: u64) -> u32 {
    if largest_row_bytes == 0 {
        return 1;
    }
    let fits = (max_inflight_bytes / largest_row_bytes).max(1);
    u32::try_from(fits.min(u64::from(batch_rows))).unwrap_or(batch_rows)
}

/// Estimativa conservadora da memória de uma célula: o slot de `DatasetCell` mais o texto.
fn cell_bytes(cell: &DatasetCell) -> u64 {
    let payload = match cell {
        DatasetCell::Text(text) => text.len() as u64,
        DatasetCell::Null | DatasetCell::Bool(_) | DatasetCell::Int(_) | DatasetCell::Float(_) => 0,
    };
    (size_of::<DatasetCell>() as u64).saturating_add(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_fetch_is_one_row_then_sized_by_largest_row() {
        assert_eq!(next_fetch_size(10_000, 4096, 0), 1);
        assert_eq!(next_fetch_size(10_000, 4096, 400), 10);
        assert_eq!(next_fetch_size(3, 4096, 400), 3);
    }

    #[test]
    fn oversized_row_still_fetches_one_row_so_the_limit_check_fails_closed() {
        assert_eq!(next_fetch_size(10, 100, 5000), 1);
    }

    #[test]
    fn limits_are_validated_before_any_query() {
        let ok = StreamLimits {
            batch_rows: 1,
            max_inflight_bytes: 1,
        };
        assert!(validate_limits(ok).is_ok());
        for invalid in [
            StreamLimits {
                batch_rows: 0,
                max_inflight_bytes: 1,
            },
            StreamLimits {
                batch_rows: STREAM_BATCH_ROWS_MAX + 1,
                max_inflight_bytes: 1,
            },
            StreamLimits {
                batch_rows: 1,
                max_inflight_bytes: 0,
            },
        ] {
            assert_eq!(
                validate_limits(invalid),
                Err(AdapterError::InvalidStreamLimit)
            );
        }
    }

    #[test]
    fn text_cells_count_their_payload() {
        let small = cell_bytes(&DatasetCell::Int(1));
        let text = cell_bytes(&DatasetCell::Text("abcdef".to_owned()));
        assert_eq!(text - small, 6);
    }
}
