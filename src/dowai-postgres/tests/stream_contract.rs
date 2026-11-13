//! Leitura em lotes (`stream_query`, ENT-031) contra PostgreSQL real.
//!
//! Opt-in: mesmas variáveis de `adapter_contract.rs` (`DOWAI_TEST_PG_*`). Use somente um banco
//! de teste descartável.

use dowai_postgres::{
    quote_identifier, AdapterError, DatasetCell, PgProfile, PostgresAdapter, RowCount, SqlAdapter,
    StreamLimits, TlsMode,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// Cria `table` com `rows` linhas de `id` e `payload` de `payload_len` caracteres.
fn create_table(adapter: &mut PostgresAdapter, table: &str, rows: i32, payload_len: i32) {
    let quoted = quote_identifier(table).expect("valid identifier");
    adapter
        .execute_sql(
            &format!(
                "CREATE TABLE {quoted} AS SELECT g::int4 AS id, repeat('x', {payload_len})::text AS payload \
                 FROM generate_series(1, {rows}) AS g"
            ),
            &[],
        )
        .expect("create table failed");
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn stream_reads_every_row_in_batches_and_reports_unknown_total() {
    let mut adapter = connect();
    let table = unique_name("dowai_stream_all");
    drop_if_exists(&mut adapter, &table);
    create_table(&mut adapter, &table, 25, 8);

    let quoted = quote_identifier(&table).expect("valid identifier");
    let mut stream = adapter
        .stream_query(
            &format!("SELECT id, payload FROM {quoted} ORDER BY id"),
            &[],
            StreamLimits {
                batch_rows: 10,
                max_inflight_bytes: 1 << 20,
            },
        )
        .expect("stream opens");
    assert_eq!(stream.total_rows(), RowCount::Unknown);
    assert_eq!(stream.columns().len(), 2);

    let mut ids = Vec::new();
    let mut batches = 0;
    while let Some(batch) = stream.next_batch().expect("batch fetch") {
        batches += 1;
        assert!(batch.rows.len() <= 10, "batch larger than batch_rows");
        for row in batch.rows {
            match row[0] {
                DatasetCell::Int(id) => ids.push(id),
                ref other => panic!("unexpected id cell: {other:?}"),
            }
        }
    }
    stream.finish().expect("stream finishes");
    drop_if_exists(&mut adapter, &table);

    assert_eq!(ids, (1..=25).collect::<Vec<i64>>());
    assert!(batches >= 3, "expected several batches, got {batches}");
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn slow_consumer_never_holds_more_than_the_inflight_byte_limit() {
    let mut adapter = connect();
    let table = unique_name("dowai_stream_slow");
    drop_if_exists(&mut adapter, &table);
    // Cada linha: ~32 bytes de célula de id + ~32 + 300 de payload ≈ 364 bytes estimados.
    create_table(&mut adapter, &table, 500, 300);

    let limit = 4096_u64;
    let quoted = quote_identifier(&table).expect("valid identifier");
    let mut stream = adapter
        .stream_query(
            &format!("SELECT id, payload FROM {quoted} ORDER BY id"),
            &[],
            StreamLimits {
                batch_rows: 1000,
                max_inflight_bytes: limit,
            },
        )
        .expect("stream opens");

    let mut delivered = 0_u64;
    let mut largest_batch = 0_u64;
    while let Some(batch) = stream.next_batch().expect("batch fetch") {
        largest_batch = largest_batch.max(batch.estimated_bytes);
        delivered += batch.rows.len() as u64;
        // Consumidor lento: enquanto segura o lote, nada além dele pode ter sido buscado.
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(
            stream.rows_read(),
            delivered,
            "rows were read ahead of the consumer"
        );
        drop(batch);
    }
    stream.finish().expect("stream finishes");
    drop_if_exists(&mut adapter, &table);

    assert_eq!(delivered, 500);
    assert!(
        largest_batch <= limit,
        "batch estimated at {largest_batch} bytes exceeds the {limit} byte limit"
    );
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn row_larger_than_the_inflight_limit_fails_closed() {
    let mut adapter = connect();
    let table = unique_name("dowai_stream_big");
    drop_if_exists(&mut adapter, &table);
    create_table(&mut adapter, &table, 3, 10_000);

    let quoted = quote_identifier(&table).expect("valid identifier");
    let mut stream = adapter
        .stream_query(
            &format!("SELECT id, payload FROM {quoted} ORDER BY id"),
            &[],
            StreamLimits {
                batch_rows: 10,
                max_inflight_bytes: 4096,
            },
        )
        .expect("stream opens");
    let error = stream.next_batch().expect_err("oversized row must fail");
    assert_eq!(error, AdapterError::InflightLimitExceeded);
    // Falha fechada: a leitura não volta a entregar linhas depois do erro.
    assert_eq!(
        stream.next_batch().expect_err("stream is failed"),
        AdapterError::QueryFailed
    );
    drop(stream);
    drop_if_exists(&mut adapter, &table);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn stream_binds_parameters_and_refuses_open_transaction() {
    let mut adapter = connect();
    let table = unique_name("dowai_stream_params");
    drop_if_exists(&mut adapter, &table);
    create_table(&mut adapter, &table, 20, 4);

    let quoted = quote_identifier(&table).expect("valid identifier");
    let limit: i32 = 15;
    let limits = StreamLimits {
        batch_rows: 100,
        max_inflight_bytes: 1 << 20,
    };
    let mut stream = adapter
        .stream_query(
            &format!("SELECT id FROM {quoted} WHERE id > $1 ORDER BY id"),
            &[&limit],
            limits,
        )
        .expect("stream opens");
    let mut count = 0;
    while let Some(batch) = stream.next_batch().expect("batch fetch") {
        count += batch.rows.len();
    }
    stream.finish().expect("stream finishes");
    assert_eq!(count, 5);

    adapter.begin_transaction().expect("begin");
    let refused = adapter
        .stream_query(&format!("SELECT id FROM {quoted}"), &[], limits)
        .map(|_| ());
    assert!(matches!(
        refused,
        Err(AdapterError::InvalidTransactionState)
    ));
    adapter.rollback().expect("rollback");
    drop_if_exists(&mut adapter, &table);
}

#[test]
#[ignore = "opt-in contract test; requires a dedicated disposable PostgreSQL database"]
fn stream_rejects_invalid_limits_before_querying() {
    let mut adapter = connect();
    let zero_rows = adapter.stream_query(
        "SELECT 1",
        &[],
        StreamLimits {
            batch_rows: 0,
            max_inflight_bytes: 1,
        },
    );
    assert!(matches!(zero_rows, Err(AdapterError::InvalidStreamLimit)));
}
