//! Full refresh com staging e troca atômica (ENT-033).
//!
//! The new relation is built as a staging relation in the destination's schema. When the
//! destination exists, its columns (name and type, in order) must match the staging relation;
//! otherwise the run fails and nothing is swapped. The swap is four statements in one
//! transaction: rename the destination aside, rename the staging relation into place, and drop
//! the old relation. Any failure drops the transaction, so the previous relation remains.

use std::time::{SystemTime, UNIX_EPOCH};

use postgres::Transaction;

use crate::{
    classify_failure, ensure_single_statement, quote_identifier, AdapterError, Materialization,
    PostgresAdapter,
};

impl PostgresAdapter {
    /// Full refresh of `relation` from `query`, observing `cancelled` like
    /// [`PostgresAdapter::materialize_cancellable`]. Replacement approval is the caller's
    /// responsibility: a full refresh is only requested with explicit confirmation.
    pub fn full_refresh_cancellable(
        &mut self,
        relation: &str,
        materialization: Materialization,
        query: &str,
        cancelled: &(dyn Fn() -> bool + Sync),
    ) -> Result<(), AdapterError> {
        self.with_cancel_watch(cancelled, |adapter| {
            adapter.full_refresh(relation, materialization, query)
        })
    }

    fn full_refresh(
        &mut self,
        relation: &str,
        materialization: Materialization,
        query: &str,
    ) -> Result<(), AdapterError> {
        ensure_single_statement(query)?;
        if query.trim().is_empty() {
            return Err(AdapterError::QueryFailed);
        }
        let quoted = quote_identifier(relation)?;
        let suffix = unique_suffix();
        let staging_name = format!("dowai_stg_{suffix}");
        let old_name = format!("dowai_old_{suffix}");
        let staging = quote_identifier(&staging_name)?;
        let old = quote_identifier(&old_name)?;
        let new_kind = match materialization {
            Materialization::Table => "TABLE",
            Materialization::View => "VIEW",
        };

        let mut transaction = self.client.transaction().map_err(classify_failure)?;
        let existing = relation_kind(&mut transaction, &quoted)?;
        transaction
            .batch_execute(&format!("CREATE {new_kind} {staging} AS {query}"))
            .map_err(classify_failure)?;

        if let Some(old_kind) = existing {
            let current = column_signature(&mut transaction, &quoted)?;
            let candidate = column_signature(&mut transaction, &staging)?;
            if current != candidate {
                return Err(AdapterError::SchemaMismatch);
            }
            transaction
                .batch_execute(&format!("ALTER {old_kind} {quoted} RENAME TO {old}"))
                .map_err(classify_failure)?;
            transaction
                .batch_execute(&format!("ALTER {new_kind} {staging} RENAME TO {quoted}"))
                .map_err(classify_failure)?;
            transaction
                .batch_execute(&format!("DROP {old_kind} {old}"))
                .map_err(classify_failure)?;
        } else {
            transaction
                .batch_execute(&format!("ALTER {new_kind} {staging} RENAME TO {quoted}"))
                .map_err(classify_failure)?;
        }
        transaction.commit().map_err(classify_failure)
    }
}

/// `TABLE` or `VIEW` keyword of the existing destination, `None` when it does not exist.
fn relation_kind(
    transaction: &mut Transaction<'_>,
    quoted: &str,
) -> Result<Option<&'static str>, AdapterError> {
    let kind = transaction
        .query_opt(
            "SELECT c.relkind::text FROM pg_class c WHERE c.oid = to_regclass($1)",
            &[&quoted],
        )
        .map_err(classify_failure)?
        .map(|row| row.get::<_, String>(0));
    match kind.as_deref() {
        None => Ok(None),
        Some("r") => Ok(Some("TABLE")),
        Some("v") => Ok(Some("VIEW")),
        Some(_) => Err(AdapterError::UnsupportedRelationType),
    }
}

/// Ordered `(column, type)` pairs of a relation, as PostgreSQL formats the types.
fn column_signature(
    transaction: &mut Transaction<'_>,
    quoted: &str,
) -> Result<Vec<(String, String)>, AdapterError> {
    transaction
        .query(
            "SELECT a.attname::text, format_type(a.atttypid, a.atttypmod) \
             FROM pg_attribute a \
             WHERE a.attrelid = to_regclass($1) AND a.attnum > 0 AND NOT a.attisdropped \
             ORDER BY a.attnum",
            &[&quoted],
        )
        .map_err(classify_failure)?
        .iter()
        .map(|row| Ok((row.get::<_, String>(0), row.get::<_, String>(1))))
        .collect()
}

/// Short, identifier-safe suffix for staging and retired names (keeps them under 63 bytes).
fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or_default();
    format!("{:x}_{:x}", std::process::id(), nanos)
}

#[cfg(test)]
mod tests {
    use super::unique_suffix;

    #[test]
    fn generated_names_fit_postgres_identifier_limit() {
        let name = format!("dowai_old_{}", unique_suffix());
        assert!(name.len() < 63, "{name} is too long");
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }
}
