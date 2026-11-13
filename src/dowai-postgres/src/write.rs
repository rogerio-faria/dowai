//! Escrita em staging e merge/upsert por chaves (ENT-032, plano §10.1–10.2).
//!
//! Fluxo dentro de uma transação aberta pelo chamador:
//! 1. [`PostgresAdapter::create_staging`] cria tabela temporária com as colunas do destino
//!    (`ON COMMIT DROP`; um rollback também a descarta);
//! 2. [`PostgresAdapter::write_staging_batch`] insere lotes parametrizados na staging;
//! 3. [`PostgresAdapter::merge_staging`] aplica `INSERT ... ON CONFLICT (chaves) DO UPDATE`
//!    do staging para o destino e devolve contagens de inseridos e atualizados.
//!
//! Idempotência: o merge é determinado pelas chaves de negócio. Reexecutar o mesmo lote com as
//! mesmas chaves atualiza linhas existentes e não cria duplicatas. Chave de idempotência em
//! nível de workflow (outbox, checkpoint) fica com ENT-048/ENT-066.
//!
//! Falhas no meio: qualquer erro deixa a transação com falha; o chamador deve chamar
//! `rollback`, que desfaz todas as escritas e descarta a staging. Transações entre bancos
//! continuam separadas (plano §10.2).

use postgres::types::ToSql;

use crate::{classify_failure, quote_identifier, AdapterError, PostgresAdapter};

/// Limite de parâmetros do protocolo PostgreSQL por instrução.
pub const WRITE_MAX_PARAMS: usize = 65_535;

/// Contagens de um merge. Linhas inalteradas por chave existente contam como `updated`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MergeCounters {
    pub inserted: u64,
    pub updated: u64,
}

/// Valida a especificação de merge antes de qualquer SQL: colunas e chaves não vazias, sem
/// duplicatas, e chaves contidas nas colunas.
pub fn validate_merge_spec(columns: &[String], keys: &[String]) -> Result<(), AdapterError> {
    if columns.is_empty() || keys.is_empty() {
        return Err(AdapterError::InvalidWriteSpec(
            "columns and keys must not be empty",
        ));
    }
    if has_duplicates(columns) || has_duplicates(keys) {
        return Err(AdapterError::InvalidWriteSpec(
            "duplicate column or key name",
        ));
    }
    if keys.iter().any(|key| !columns.contains(key)) {
        return Err(AdapterError::InvalidWriteSpec(
            "merge key is not a written column",
        ));
    }
    Ok(())
}

/// Valida um lote de escrita: colunas não vazias e linhas com o número de valores das colunas,
/// dentro do limite de parâmetros.
pub fn validate_batch_shape(columns: &[String], row_lengths: &[usize]) -> Result<(), AdapterError> {
    if columns.is_empty() {
        return Err(AdapterError::InvalidWriteSpec(
            "batch must declare at least one column",
        ));
    }
    if row_lengths.iter().any(|length| *length != columns.len()) {
        return Err(AdapterError::InvalidWriteSpec(
            "row length differs from column count",
        ));
    }
    if row_lengths.len().saturating_mul(columns.len()) > WRITE_MAX_PARAMS {
        return Err(AdapterError::InvalidWriteSpec(
            "batch exceeds the bound parameter limit",
        ));
    }
    Ok(())
}

fn has_duplicates(names: &[String]) -> bool {
    names
        .iter()
        .enumerate()
        .any(|(index, name)| names[..index].contains(name))
}

fn quoted_list(names: &[String]) -> Result<String, AdapterError> {
    let quoted = names
        .iter()
        .map(|name| quote_identifier(name))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(quoted.join(", "))
}

impl PostgresAdapter {
    /// Cria a staging temporária com as mesmas colunas e defaults de `target`. Exige transação.
    pub fn create_staging(&mut self, target: &str, staging: &str) -> Result<(), AdapterError> {
        self.require_transaction()?;
        let target = quote_identifier(target)?;
        let staging = quote_identifier(staging)?;
        self.client
            .batch_execute(&format!(
                "CREATE TEMP TABLE {staging} (LIKE {target} INCLUDING DEFAULTS) ON COMMIT DROP"
            ))
            .map_err(classify_failure)
    }

    /// Insere um lote parametrizado na staging. Cada linha tem um valor por coluna, na ordem
    /// de `columns`. Exige transação. Devolve as linhas inseridas.
    pub fn write_staging_batch(
        &mut self,
        staging: &str,
        columns: &[String],
        rows: &[Vec<&(dyn ToSql + Sync)>],
    ) -> Result<u64, AdapterError> {
        self.require_transaction()?;
        let row_lengths: Vec<usize> = rows.iter().map(Vec::len).collect();
        validate_batch_shape(columns, &row_lengths)?;
        if rows.is_empty() {
            return Ok(0);
        }
        let staging = quote_identifier(staging)?;
        let columns_sql = quoted_list(columns)?;
        let mut placeholder = 0usize;
        let values = rows
            .iter()
            .map(|row| {
                let cells = row
                    .iter()
                    .map(|_| {
                        placeholder += 1;
                        format!("${placeholder}")
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("({cells})")
            })
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("INSERT INTO {staging} ({columns_sql}) VALUES {values}");
        let params: Vec<&(dyn ToSql + Sync)> =
            rows.iter().flat_map(|row| row.iter().copied()).collect();
        self.client
            .execute(sql.as_str(), &params)
            .map_err(classify_failure)
    }

    /// Aplica o staging ao destino por upsert nas `keys`. Exige transação. Falha se a staging
    /// tiver chaves repetidas (`DuplicateMergeKey`) ou se o destino não tiver restrição única
    /// sobre `keys` (erro de SQL, permanente).
    pub fn merge_staging(
        &mut self,
        staging: &str,
        target: &str,
        columns: &[String],
        keys: &[String],
    ) -> Result<MergeCounters, AdapterError> {
        self.require_transaction()?;
        validate_merge_spec(columns, keys)?;
        let staging_q = quote_identifier(staging)?;
        let target_q = quote_identifier(target)?;
        let key_list = quoted_list(keys)?;
        let columns_sql = quoted_list(columns)?;

        let duplicate_keys = self
            .client
            .query_one(
                &format!(
                    "SELECT count(*) FROM (SELECT 1 FROM {staging_q} GROUP BY {key_list} \
                     HAVING count(*) > 1) AS repeated"
                ),
                &[],
            )
            .map_err(classify_failure)?
            .get::<_, i64>(0);
        if duplicate_keys > 0 {
            return Err(AdapterError::DuplicateMergeKey);
        }

        let non_keys: Vec<&String> = columns.iter().filter(|name| !keys.contains(name)).collect();
        let conflict_action = if non_keys.is_empty() {
            "DO NOTHING".to_owned()
        } else {
            let assignments = non_keys
                .iter()
                .map(|name| {
                    let quoted = quote_identifier(name)?;
                    Ok(format!("{quoted} = EXCLUDED.{quoted}"))
                })
                .collect::<Result<Vec<_>, AdapterError>>()?
                .join(", ");
            format!("DO UPDATE SET {assignments}")
        };
        // `xmax = 0` identifica a versão recém-inserida; versões criadas por DO UPDATE têm xmax
        // preenchido pela transação corrente.
        let sql = format!(
            "WITH upserted AS (\
                INSERT INTO {target_q} ({columns_sql}) SELECT {columns_sql} FROM {staging_q} \
                ON CONFLICT ({key_list}) {conflict_action} \
                RETURNING (xmax = 0) AS inserted\
             ) \
             SELECT count(*) FILTER (WHERE inserted), count(*) FILTER (WHERE NOT inserted) \
             FROM upserted"
        );
        let row = self
            .client
            .query_one(sql.as_str(), &[])
            .map_err(classify_failure)?;
        let inserted: i64 = row.get(0);
        let touched: i64 = row.get(1);
        Ok(MergeCounters {
            inserted: u64::try_from(inserted).unwrap_or(0),
            updated: u64::try_from(touched).unwrap_or(0),
        })
    }

    /// Insere linhas JSON (`[{coluna: valor}, ...]`) na staging. O PostgreSQL converte cada
    /// valor para o tipo da coluna de `staging` (mesmas colunas do destino), sem interpolar
    /// dados no SQL: o JSON é um único parâmetro. Exige transação. Devolve linhas inseridas.
    pub fn write_staging_json(
        &mut self,
        staging: &str,
        columns: &[String],
        rows_json: &str,
    ) -> Result<u64, AdapterError> {
        self.require_transaction()?;
        validate_batch_shape(columns, &[])?;
        let staging_q = quote_identifier(staging)?;
        let columns_sql = quoted_list(columns)?;
        let sql = format!(
            "INSERT INTO {staging_q} ({columns_sql}) SELECT {columns_sql} \
             FROM jsonb_populate_recordset(NULL::{staging_q}, $1::text::jsonb)"
        );
        self.client
            .execute(sql.as_str(), &[&rows_json])
            .map_err(classify_failure)
    }

    /// Copia a staging para o destino sem conflito (`append`). Exige transação. Devolve linhas
    /// inseridas.
    pub fn append_staging(
        &mut self,
        staging: &str,
        target: &str,
        columns: &[String],
    ) -> Result<u64, AdapterError> {
        self.require_transaction()?;
        validate_batch_shape(columns, &[])?;
        let staging_q = quote_identifier(staging)?;
        let target_q = quote_identifier(target)?;
        let columns_sql = quoted_list(columns)?;
        self.client
            .execute(
                &format!(
                    "INSERT INTO {target_q} ({columns_sql}) SELECT {columns_sql} FROM {staging_q}"
                ),
                &[],
            )
            .map_err(classify_failure)
    }

    /// Remove todas as linhas do destino (`replace`, dentro da transação). Exige transação.
    pub fn delete_all(&mut self, target: &str) -> Result<u64, AdapterError> {
        self.require_transaction()?;
        let target_q = quote_identifier(target)?;
        self.client
            .execute(&format!("DELETE FROM {target_q}"), &[])
            .map_err(classify_failure)
    }

    /// Indica se `target` tem índice único cujas colunas são exatamente `keys` (sem parcial,
    /// expressão ou colunas `INCLUDE`). Pré-condição de `upsert`. Não exige transação.
    pub fn has_unique_key(&mut self, target: &str, keys: &[String]) -> Result<bool, AdapterError> {
        let target_q = quote_identifier(target)?;
        let mut expected: Vec<&String> = keys.iter().collect();
        expected.sort();
        let rows = self
            .client
            .query(
                "SELECT array_agg(a.attname::text ORDER BY a.attname::text)::text[] \
                 FROM pg_index i \
                 JOIN pg_attribute a ON a.attrelid = i.indrelid \
                   AND a.attnum = ANY(string_to_array(i.indkey::text, ' ')::int2[]) \
                 WHERE i.indrelid = to_regclass($1::text) AND i.indisunique \
                   AND i.indpred IS NULL AND i.indexprs IS NULL AND i.indnatts = i.indnkeyatts \
                 GROUP BY i.indexrelid",
                &[&target_q],
            )
            .map_err(classify_failure)?;
        Ok(rows.iter().any(|row| {
            let mut names: Vec<String> = row.get(0);
            names.sort();
            names.len() == expected.len() && names.iter().zip(expected.iter()).all(|(a, b)| a == *b)
        }))
    }

    fn require_transaction(&self) -> Result<(), AdapterError> {
        if self.in_transaction {
            Ok(())
        } else {
            Err(AdapterError::InvalidTransactionState)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn merge_spec_requires_keys_inside_columns() {
        assert!(validate_merge_spec(&names(&["id", "name"]), &names(&["id"])).is_ok());
        assert_eq!(
            validate_merge_spec(&names(&["name"]), &names(&["id"])),
            Err(AdapterError::InvalidWriteSpec(
                "merge key is not a written column"
            ))
        );
        assert!(validate_merge_spec(&names(&["id"]), &[]).is_err());
        assert!(validate_merge_spec(&names(&["id", "id"]), &names(&["id"])).is_err());
    }

    #[test]
    fn batch_shape_rejects_ragged_rows_and_excess_parameters() {
        let columns = names(&["a", "b"]);
        assert!(validate_batch_shape(&columns, &[2, 2]).is_ok());
        assert!(validate_batch_shape(&columns, &[2, 1]).is_err());
        assert!(validate_batch_shape(&[], &[]).is_err());
        let too_many = vec![2usize; WRITE_MAX_PARAMS];
        assert!(validate_batch_shape(&columns, &too_many).is_err());
    }
}
