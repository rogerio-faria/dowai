use dowai_postgres::{quote_identifier, Materialization, PgProfile, PostgresAdapter, TlsMode};
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

#[test]
#[ignore = "opt-in smoke test; requires a dedicated PostgreSQL instance"]
fn live_connection_executes_read_only_select_one() {
    let profile = configured_profile();
    let mut adapter = PostgresAdapter::connect(&profile).expect("PostgreSQL connection failed");
    adapter
        .test_connection()
        .expect("read-only PostgreSQL smoke query failed");
}

#[test]
#[ignore = "opt-in destructive smoke test; requires a dedicated disposable PostgreSQL database"]
fn live_connection_replaces_table_and_view_transactionally() {
    let profile = configured_profile();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after Unix epoch")
        .as_nanos();
    let relation = format!("dowai_smoke_{}_{}", std::process::id(), nonce);
    let quoted = quote_identifier(&relation).expect("generated relation is valid");

    let mut adapter = PostgresAdapter::connect(&profile).expect("PostgreSQL connection failed");
    adapter
        .materialize(&relation, Materialization::Table, "SELECT 1 AS value", true)
        .expect("create test table");
    adapter
        .materialize(&relation, Materialization::Table, "SELECT 2 AS value", true)
        .expect("replace test table");
    adapter
        .materialize(&relation, Materialization::View, "SELECT 3 AS value", true)
        .expect("replace test table with a view");
    let rows = adapter
        .query(&format!("SELECT value FROM {quoted}"), &[])
        .expect("read materialized view");
    assert_eq!(rows[0].get::<_, i32>(0), 3);
    adapter
        .drop_relation(&relation, true)
        .expect("remove smoke-test view");
}
