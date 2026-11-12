//! Contrato `SqlAdapter` executado contra PostgreSQL real (ENT-029).
//!
//! Opt-in: exige `DOWAI_TEST_PG_HOST`, `DOWAI_TEST_PG_PORT`, `DOWAI_TEST_PG_USER`,
//! `DOWAI_TEST_PG_DATABASE` e `DOWAI_TEST_PG_PASSWORD` (e `DOWAI_TEST_PG_TLS=disabled` para
//! cluster local sem TLS). Use somente um banco de teste descartável.

use dowai_postgres::{
    quote_identifier, AdapterError, PgProfile, PostgresAdapter, SqlAdapter, TlsMode,
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

fn drop_if_exists(adapter: &mut PostgresAdapter, table: &str) {
    let quoted = quote_identifier(table).expect("valid identifier");
    adapter
        .execute_sql(&format!("DROP TABLE IF EXISTS {quoted}"), &[])
        .expect("cleanup failed");
}

fn count_rows(adapter: &mut PostgresAdapter, table: &str) -> i64 {
    let quoted = quote_identifier(table).expect("valid identifier");
    let rows = adapter
        .query(&format!("SELECT count(*) FROM {quoted}"), &[])
        .expect("count query failed");
    rows[0].get::<_, i64>(0)
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn contract_test_connection_returns_declared_capabilities() {
    let mut adapter = connect();
    let declared = adapter.capabilities();
    let returned = SqlAdapter::test_connection(&mut adapter).expect("test_connection failed");
    assert_eq!(declared, returned);
    assert!(returned.transactional_ddl);
    assert!(returned.parameters);
    assert!(returned.row_count);
    assert!(!returned.stream, "stream is not implemented before ENT-031");
    assert!(
        !returned.merge_upsert,
        "merge is not implemented before ENT-032"
    );
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn contract_introspect_returns_column_names_and_types() {
    let mut adapter = connect();
    let table = unique_name("dowai_contract_intro");
    let quoted = quote_identifier(&table).expect("valid identifier");
    adapter
        .execute_sql(
            &format!("CREATE TABLE {quoted} (id integer, label text, amount numeric(12,2))"),
            &[],
        )
        .expect("create failed");

    let columns = adapter.introspect_table(&table).expect("introspect failed");
    let names: Vec<(&str, &str)> = columns
        .iter()
        .map(|column| (column.name.as_str(), column.data_type.as_str()))
        .collect();
    assert_eq!(
        names,
        vec![("id", "int4"), ("label", "text"), ("amount", "numeric")]
    );

    drop_if_exists(&mut adapter, &table);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn contract_quoting_handles_quote_characters_in_identifiers() {
    let mut adapter = connect();
    let table = format!("{}\"quoted", unique_name("dowai_contract_quote"));
    let quoted = quote_identifier(&table).expect("valid identifier");
    adapter
        .execute_sql(&format!("CREATE TABLE {quoted} (v integer)"), &[])
        .expect("create failed");

    let columns = adapter.introspect_table(&table).expect("introspect failed");
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].name, "v");

    drop_if_exists(&mut adapter, &table);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn contract_parameters_are_bound_and_row_count_is_reported() {
    let mut adapter = connect();
    let table = unique_name("dowai_contract_rows");
    let quoted = quote_identifier(&table).expect("valid identifier");
    adapter
        .execute_sql(&format!("CREATE TABLE {quoted} (v text)"), &[])
        .expect("create failed");

    let value = "'); DROP TABLE x; --";
    let affected = adapter
        .execute_sql(
            &format!("INSERT INTO {quoted} (v) VALUES ($1), ($2)"),
            &[&value, &"segundo"],
        )
        .expect("insert failed");
    assert_eq!(affected, 2, "row count must match the affected rows");
    assert_eq!(count_rows(&mut adapter, &table), 2);

    let stored = adapter
        .query(&format!("SELECT v FROM {quoted} ORDER BY v"), &[])
        .expect("select failed");
    let values: Vec<String> = stored.iter().map(|row| row.get::<_, String>(0)).collect();
    assert!(
        values.contains(&value.to_owned()),
        "parameter must be stored verbatim"
    );

    drop_if_exists(&mut adapter, &table);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn contract_rollback_discards_writes_and_ddl() {
    let mut adapter = connect();
    let table = unique_name("dowai_contract_rb");
    let quoted = quote_identifier(&table).expect("valid identifier");

    adapter.begin_transaction().expect("begin failed");
    adapter
        .execute_sql(&format!("CREATE TABLE {quoted} (v integer)"), &[])
        .expect("create in transaction failed");
    adapter
        .execute_sql(&format!("INSERT INTO {quoted} (v) VALUES (1), (2)"), &[])
        .expect("insert in transaction failed");
    adapter.rollback().expect("rollback failed");

    let error = adapter
        .introspect_table(&table)
        .expect_err("table created in a rolled-back transaction must not exist");
    assert_eq!(error, AdapterError::QueryFailed);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn contract_commit_persists_writes() {
    let mut adapter = connect();
    let table = unique_name("dowai_contract_cm");
    let quoted = quote_identifier(&table).expect("valid identifier");
    adapter
        .execute_sql(&format!("CREATE TABLE {quoted} (v integer)"), &[])
        .expect("create failed");

    adapter.begin_transaction().expect("begin failed");
    adapter
        .execute_sql(&format!("INSERT INTO {quoted} (v) VALUES (7)"), &[])
        .expect("insert failed");
    adapter.commit().expect("commit failed");

    assert_eq!(count_rows(&mut adapter, &table), 1);
    drop_if_exists(&mut adapter, &table);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn contract_invalid_transaction_state_is_rejected() {
    let mut adapter = connect();
    assert_eq!(
        adapter.commit(),
        Err(AdapterError::InvalidTransactionState),
        "commit without an open transaction must be rejected"
    );
    assert_eq!(
        adapter.rollback(),
        Err(AdapterError::InvalidTransactionState),
        "rollback without an open transaction must be rejected"
    );

    adapter.begin_transaction().expect("begin failed");
    assert_eq!(
        adapter.begin_transaction(),
        Err(AdapterError::InvalidTransactionState),
        "nested begin must be rejected"
    );
    adapter.rollback().expect("rollback failed");
}
