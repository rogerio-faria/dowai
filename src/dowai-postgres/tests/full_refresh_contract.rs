//! Contrato de full refresh com staging e troca atômica contra PostgreSQL real (ENT-033).
//!
//! Opt-in: exige `DOWAI_TEST_PG_HOST`, `DOWAI_TEST_PG_PORT`, `DOWAI_TEST_PG_USER`,
//! `DOWAI_TEST_PG_DATABASE` e `DOWAI_TEST_PG_PASSWORD` (e `DOWAI_TEST_PG_TLS=disabled` para
//! cluster local sem TLS). Use somente um banco de teste descartável. Os testes assumem
//! execução com `--test-threads=1`, porque verificam a ausência de relações residuais.

use dowai_postgres::{
    quote_identifier, AdapterError, Materialization, PgProfile, PostgresAdapter, SqlAdapter,
    TlsMode,
};
use std::time::{SystemTime, UNIX_EPOCH};

const SAME_SCHEMA_QUERY: &str = "SELECT 2 AS id, 'novo'::text AS name, 20 AS amount \
     UNION ALL SELECT 3, 'outro', 30";

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

fn exec(adapter: &mut PostgresAdapter, sql: &str) {
    adapter.execute_sql(sql, &[]).expect("setup statement");
}

fn drop_relation(adapter: &mut PostgresAdapter, name: &str) {
    let quoted = quote_identifier(name).expect("valid identifier");
    let kind: Option<String> = adapter
        .query(
            "SELECT c.relkind::text FROM pg_class c WHERE c.oid = to_regclass($1)",
            &[&quoted],
        )
        .expect("kind query")
        .first()
        .map(|row| row.get(0));
    match kind.as_deref() {
        Some("v") => exec(adapter, &format!("DROP VIEW {quoted}")),
        Some("r") => exec(adapter, &format!("DROP TABLE {quoted}")),
        _ => {}
    }
}

fn create_target(adapter: &mut PostgresAdapter, name: &str) {
    let quoted = quote_identifier(name).expect("valid identifier");
    exec(
        adapter,
        &format!("CREATE TABLE {quoted} AS SELECT 1 AS id, 'antigo'::text AS name, 10 AS amount"),
    );
}

fn snapshot(adapter: &mut PostgresAdapter, name: &str) -> Vec<(i32, String, i32)> {
    let quoted = quote_identifier(name).expect("valid identifier");
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

/// Staging and retired relations left behind in the database (names built by the adapter).
fn residual_relations(adapter: &mut PostgresAdapter) -> i64 {
    adapter
        .query(
            "SELECT count(*) FROM pg_class \
             WHERE relname LIKE 'dowai\\_stg\\_%' OR relname LIKE 'dowai\\_old\\_%'",
            &[],
        )
        .expect("residual query")[0]
        .get(0)
}

fn full_refresh(
    adapter: &mut PostgresAdapter,
    name: &str,
    materialization: Materialization,
    query: &str,
) -> Result<(), AdapterError> {
    adapter.full_refresh_cancellable(name, materialization, query, &|| false)
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn full_refresh_creates_missing_table_without_residual_relations() {
    let mut adapter = connect();
    let name = unique_name("dowai_fr_create");
    drop_relation(&mut adapter, &name);

    full_refresh(
        &mut adapter,
        &name,
        Materialization::Table,
        SAME_SCHEMA_QUERY,
    )
    .expect("full refresh of a missing table");

    assert_eq!(snapshot(&mut adapter, &name).len(), 2);
    assert_eq!(residual_relations(&mut adapter), 0);
    drop_relation(&mut adapter, &name);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn full_refresh_replaces_rows_when_columns_match() {
    let mut adapter = connect();
    let name = unique_name("dowai_fr_replace");
    drop_relation(&mut adapter, &name);
    create_target(&mut adapter, &name);

    full_refresh(
        &mut adapter,
        &name,
        Materialization::Table,
        SAME_SCHEMA_QUERY,
    )
    .expect("full refresh with matching schema");

    assert_eq!(
        snapshot(&mut adapter, &name),
        vec![(2, "novo".to_owned(), 20), (3, "outro".to_owned(), 30)]
    );
    assert_eq!(residual_relations(&mut adapter), 0);
    drop_relation(&mut adapter, &name);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn full_refresh_rejects_divergent_schema_and_keeps_destination() {
    let mut adapter = connect();
    let name = unique_name("dowai_fr_schema");
    drop_relation(&mut adapter, &name);
    create_target(&mut adapter, &name);

    let result = full_refresh(
        &mut adapter,
        &name,
        Materialization::Table,
        "SELECT 9 AS id, 'x'::text AS label",
    );

    assert_eq!(result, Err(AdapterError::SchemaMismatch));
    assert_eq!(
        snapshot(&mut adapter, &name),
        vec![(1, "antigo".to_owned(), 10)]
    );
    assert_eq!(residual_relations(&mut adapter), 0);
    drop_relation(&mut adapter, &name);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn full_refresh_failure_while_building_staging_keeps_previous_relation() {
    let mut adapter = connect();
    let name = unique_name("dowai_fr_stage_fail");
    drop_relation(&mut adapter, &name);
    create_target(&mut adapter, &name);

    let result = full_refresh(
        &mut adapter,
        &name,
        Materialization::Table,
        "SELECT 1 / 0 AS id, 'x'::text AS name, 1 AS amount",
    );

    assert_eq!(result, Err(AdapterError::QueryFailed));
    assert_eq!(
        snapshot(&mut adapter, &name),
        vec![(1, "antigo".to_owned(), 10)]
    );
    assert_eq!(residual_relations(&mut adapter), 0);
    drop_relation(&mut adapter, &name);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn full_refresh_failure_during_swap_rolls_back_to_previous_relation() {
    let mut adapter = connect();
    let name = unique_name("dowai_fr_swap_fail");
    let dependent = format!("{name}_dep");
    drop_relation(&mut adapter, &dependent);
    drop_relation(&mut adapter, &name);
    create_target(&mut adapter, &name);
    // A view on the destination makes the final DROP of the retired relation fail, after
    // both renames already ran inside the same transaction.
    exec(
        &mut adapter,
        &format!(
            "CREATE VIEW {} AS SELECT id FROM {}",
            quote_identifier(&dependent).expect("valid identifier"),
            quote_identifier(&name).expect("valid identifier")
        ),
    );

    let result = full_refresh(
        &mut adapter,
        &name,
        Materialization::Table,
        SAME_SCHEMA_QUERY,
    );

    assert_eq!(result, Err(AdapterError::QueryFailed));
    assert_eq!(
        snapshot(&mut adapter, &name),
        vec![(1, "antigo".to_owned(), 10)]
    );
    assert_eq!(residual_relations(&mut adapter), 0);
    drop_relation(&mut adapter, &dependent);
    drop_relation(&mut adapter, &name);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn full_refresh_replaces_view_definition() {
    let mut adapter = connect();
    let name = unique_name("dowai_fr_view");
    drop_relation(&mut adapter, &name);
    exec(
        &mut adapter,
        &format!(
            "CREATE VIEW {} AS SELECT 1 AS id, 'antigo'::text AS name, 10 AS amount",
            quote_identifier(&name).expect("valid identifier")
        ),
    );

    full_refresh(
        &mut adapter,
        &name,
        Materialization::View,
        SAME_SCHEMA_QUERY,
    )
    .expect("full refresh of a view");

    assert_eq!(
        snapshot(&mut adapter, &name),
        vec![(2, "novo".to_owned(), 20), (3, "outro".to_owned(), 30)]
    );
    assert_eq!(residual_relations(&mut adapter), 0);
    drop_relation(&mut adapter, &name);
}
