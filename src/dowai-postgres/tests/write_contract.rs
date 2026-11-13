//! Contrato de escrita em staging e merge/upsert contra PostgreSQL real (ENT-032).
//!
//! Opt-in: exige `DOWAI_TEST_PG_HOST`, `DOWAI_TEST_PG_PORT`, `DOWAI_TEST_PG_USER`,
//! `DOWAI_TEST_PG_DATABASE` e `DOWAI_TEST_PG_PASSWORD` (e `DOWAI_TEST_PG_TLS=disabled` para
//! cluster local sem TLS). Use somente um banco de teste descartável.

use dowai_postgres::{
    quote_identifier, AdapterError, MergeCounters, PgProfile, PostgresAdapter, SqlAdapter, TlsMode,
};
use std::time::{SystemTime, UNIX_EPOCH};

fn configured_profile() -> PgProfile {
    let host = std::env::var("DOWAI_TEST_PG_HOST").expect("DOWAI_TEST_PG_HOST is required");
    let port = std::env::var("DOWAI_TEST_PG_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .expect("DOWAI_TEST_PG_PORT must be a valid port");
    let user = std::env::var("DOWAI_TEST_PG_USER").expect("DOWAI_TEST_PG_USER is required");
    let database =
        std::env::var("DOWAI_TEST_PG_DATABASE").expect("DOWAI_TEST_PG_DATABASE is required");
    let mut profile = PgProfile::new(host, port, user, database, "DOWAI_TEST_PG_PASSWORD");
    if std::env::var("DOWAI_TEST_PG_TLS").as_deref() == Ok("disabled") {
        profile.set_tls_mode(TlsMode::Disabled);
    }
    profile
}

fn unique_name(prefix: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after Unix epoch")
        .as_nanos();
    format!("{prefix}_{}_{nonce}", std::process::id())
}

fn connect() -> PostgresAdapter {
    PostgresAdapter::connect(&configured_profile()).expect("PostgreSQL connection failed")
}

fn names(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn create_target(adapter: &mut PostgresAdapter, table: &str) {
    let quoted = quote_identifier(table).expect("valid identifier");
    adapter
        .execute_sql(
            &format!(
                "CREATE TABLE {quoted} (id integer PRIMARY KEY, name text NOT NULL, \
                 amount integer NOT NULL DEFAULT 0)"
            ),
            &[],
        )
        .expect("create target");
}

fn drop_table(adapter: &mut PostgresAdapter, table: &str) {
    let quoted = quote_identifier(table).expect("valid identifier");
    adapter
        .execute_sql(&format!("DROP TABLE IF EXISTS {quoted}"), &[])
        .expect("cleanup failed");
}

fn snapshot(adapter: &mut PostgresAdapter, table: &str) -> Vec<(i32, String, i32)> {
    let quoted = quote_identifier(table).expect("valid identifier");
    adapter
        .query(
            &format!("SELECT id, name, amount FROM {quoted} ORDER BY id"),
            &[],
        )
        .expect("snapshot query")
        .iter()
        .map(|row| (row.get(0), row.get(1), row.get(2)))
        .collect()
}

/// Staging + merge numa transação; devolve os contadores do merge.
fn merge_once(
    adapter: &mut PostgresAdapter,
    target: &str,
    staging: &str,
    columns: &[String],
    keys: &[String],
    rows: &[(i32, &str, i32)],
) -> Result<MergeCounters, AdapterError> {
    SqlAdapter::begin_transaction(adapter)?;
    let outcome = (|| {
        SqlAdapter::create_staging(adapter, target, staging)?;
        let owned: Vec<(i32, String, i32)> = rows
            .iter()
            .map(|(id, name, amount)| (*id, (*name).to_owned(), *amount))
            .collect();
        let values: Vec<Vec<&(dyn postgres::types::ToSql + Sync)>> = owned
            .iter()
            .map(|(id, name, amount)| {
                vec![
                    id as &(dyn postgres::types::ToSql + Sync),
                    name as &(dyn postgres::types::ToSql + Sync),
                    amount as &(dyn postgres::types::ToSql + Sync),
                ]
            })
            .collect();
        SqlAdapter::write_staging_batch(adapter, staging, columns, &values)?;
        SqlAdapter::merge(adapter, staging, target, columns, keys)
    })();
    match outcome {
        Ok(counters) => {
            SqlAdapter::commit(adapter)?;
            Ok(counters)
        }
        Err(error) => {
            SqlAdapter::rollback(adapter)?;
            Err(error)
        }
    }
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn merge_inserts_new_keys_and_updates_existing_ones() {
    let target = unique_name("dowai_merge_target");
    let staging = unique_name("dowai_merge_stage");
    let columns = names(&["id", "name", "amount"]);
    let keys = names(&["id"]);
    let mut adapter = connect();
    create_target(&mut adapter, &target);
    SqlAdapter::execute_sql(
        &mut adapter,
        &format!(
            "INSERT INTO {} VALUES (1, 'antigo', 10)",
            quote_identifier(&target).expect("valid identifier")
        ),
        &[],
    )
    .expect("seed row");

    let counters = merge_once(
        &mut adapter,
        &target,
        &staging,
        &columns,
        &keys,
        &[(1, "novo", 11), (2, "dois", 20)],
    )
    .expect("merge");

    assert_eq!(
        counters,
        MergeCounters {
            inserted: 1,
            updated: 1
        }
    );
    assert_eq!(
        snapshot(&mut adapter, &target),
        vec![(1, "novo".to_owned(), 11), (2, "dois".to_owned(), 20)]
    );
    drop_table(&mut adapter, &target);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn rerunning_the_same_batch_does_not_duplicate_rows() {
    let target = unique_name("dowai_merge_idem");
    let staging = unique_name("dowai_merge_idem_stage");
    let columns = names(&["id", "name", "amount"]);
    let keys = names(&["id"]);
    let batch = [(1, "a", 1), (2, "b", 2), (3, "c", 3)];
    let mut adapter = connect();
    create_target(&mut adapter, &target);

    let first =
        merge_once(&mut adapter, &target, &staging, &columns, &keys, &batch).expect("first merge");
    let second =
        merge_once(&mut adapter, &target, &staging, &columns, &keys, &batch).expect("second merge");

    assert_eq!(
        first,
        MergeCounters {
            inserted: 3,
            updated: 0
        }
    );
    assert_eq!(
        second,
        MergeCounters {
            inserted: 0,
            updated: 3
        }
    );
    assert_eq!(snapshot(&mut adapter, &target).len(), 3);
    drop_table(&mut adapter, &target);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn failure_in_the_middle_preserves_the_destination() {
    let target = unique_name("dowai_merge_fail");
    let staging = unique_name("dowai_merge_fail_stage");
    let columns = names(&["id", "name", "amount"]);
    let keys = names(&["id"]);
    let mut adapter = connect();
    create_target(&mut adapter, &target);
    merge_once(
        &mut adapter,
        &target,
        &staging,
        &columns,
        &keys,
        &[(1, "orig", 5)],
    )
    .expect("seed merge");
    let before = snapshot(&mut adapter, &target);

    // O merge de um lote válido já foi aplicado; em seguida um segundo lote com NULL em
    // coluna NOT NULL falha. O rollback deve desfazer também o merge anterior.
    SqlAdapter::begin_transaction(&mut adapter).expect("begin");
    SqlAdapter::create_staging(&mut adapter, &target, &staging).expect("staging");
    let good = (2i32, "ok".to_owned(), 7i32);
    let good_values: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&good.0, &good.1, &good.2];
    SqlAdapter::write_staging_batch(&mut adapter, &staging, &columns, &[good_values])
        .expect("valid batch");
    let applied = SqlAdapter::merge(&mut adapter, &staging, &target, &columns, &keys)
        .expect("merge of valid batch");
    assert_eq!(
        applied,
        MergeCounters {
            inserted: 1,
            updated: 0
        }
    );
    let bad_name: Option<String> = None;
    let bad = (3i32, bad_name, 9i32);
    let bad_values: Vec<&(dyn postgres::types::ToSql + Sync)> = vec![&bad.0, &bad.1, &bad.2];
    SqlAdapter::write_staging_batch(&mut adapter, &staging, &columns, &[bad_values])
        .expect_err("NOT NULL violation must fail");
    SqlAdapter::rollback(&mut adapter).expect("rollback");

    assert_eq!(snapshot(&mut adapter, &target), before);
    drop_table(&mut adapter, &target);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn duplicate_keys_in_staging_are_rejected_without_writing() {
    let target = unique_name("dowai_merge_dup");
    let staging = unique_name("dowai_merge_dup_stage");
    let columns = names(&["id", "name", "amount"]);
    let keys = names(&["id"]);
    let mut adapter = connect();
    create_target(&mut adapter, &target);

    let result = merge_once(
        &mut adapter,
        &target,
        &staging,
        &columns,
        &keys,
        &[(1, "x", 1), (1, "y", 2)],
    );

    assert_eq!(result, Err(AdapterError::DuplicateMergeKey));
    assert!(snapshot(&mut adapter, &target).is_empty());
    drop_table(&mut adapter, &target);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn staging_and_merge_require_an_open_transaction() {
    let target = unique_name("dowai_merge_tx");
    let staging = unique_name("dowai_merge_tx_stage");
    let columns = names(&["id", "name", "amount"]);
    let keys = names(&["id"]);
    let mut adapter = connect();
    create_target(&mut adapter, &target);

    assert_eq!(
        SqlAdapter::create_staging(&mut adapter, &target, &staging),
        Err(AdapterError::InvalidTransactionState)
    );
    assert_eq!(
        SqlAdapter::merge(&mut adapter, &staging, &target, &columns, &keys),
        Err(AdapterError::InvalidTransactionState)
    );
    drop_table(&mut adapter, &target);
}
