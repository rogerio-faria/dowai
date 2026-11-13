//! Contrato do adapter MySQL contra MySQL real (ENT-034).
//!
//! Opt-in (`--ignored`): exige `DOWAI_TEST_MYSQL_HOST`, `_PORT`, `_USER`, `_PASSWORD` e
//! `_DATABASE` (banco descartável; `DOWAI_TEST_MYSQL_TLS=required` liga TLS, o padrão dos testes
//! é TLS desligado em loopback). Cada teste cria tabelas com nome único e as remove ao fim.

use dowai_mysql::{
    quote_identifier, MysqlAdapter, MysqlError, MysqlProfile, Value, MYSQL_CAPABILITIES,
};
use dowai_postgres::{DatasetCell, StreamLimits, TlsMode};
use std::time::{SystemTime, UNIX_EPOCH};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn profile() -> MysqlProfile {
    let mut profile = MysqlProfile::new(
        env("DOWAI_TEST_MYSQL_HOST"),
        env("DOWAI_TEST_MYSQL_PORT").parse().expect("valid port"),
        env("DOWAI_TEST_MYSQL_USER"),
        env("DOWAI_TEST_MYSQL_DATABASE"),
        "DOWAI_TEST_MYSQL_PASSWORD",
    );
    if std::env::var("DOWAI_TEST_MYSQL_TLS").as_deref() != Ok("required") {
        profile.set_tls_mode(TlsMode::Disabled);
    }
    profile
}

fn connect() -> MysqlAdapter {
    MysqlAdapter::connect(&profile()).expect("MySQL connection failed")
}

fn unique(prefix: &str) -> String {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("{prefix}_{}_{}", std::process::id(), nonce % 1_000_000_000)
}

fn names(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

/// Remove a tabela ao sair, mesmo se uma asserção falhar.
struct Table(String);

impl Table {
    fn create(adapter: &mut MysqlAdapter, prefix: &str, columns: &str) -> Self {
        let name = unique(prefix);
        let quoted = quote_identifier(&name).expect("identifier");
        adapter
            .execute_sql(&format!("CREATE TABLE {quoted} ({columns})"), &[])
            .expect("create table");
        Self(name)
    }

    fn q(&self) -> String {
        quote_identifier(&self.0).expect("identifier")
    }
}

impl Drop for Table {
    fn drop(&mut self) {
        if let Ok(mut adapter) = MysqlAdapter::connect(&profile()) {
            let _ = adapter.execute_sql(&format!("DROP TABLE IF EXISTS {}", self.q()), &[]);
        }
    }
}

const PEOPLE: &str = "id INT PRIMARY KEY, name VARCHAR(20) NOT NULL, amount INT NOT NULL DEFAULT 0";

fn count(adapter: &mut MysqlAdapter, table: &Table) -> i64 {
    match adapter
        .query_rows(&format!("SELECT COUNT(*) FROM {}", table.q()), &[])
        .expect("count")[0][0]
    {
        DatasetCell::Int(value) => value,
        ref other => panic!("unexpected count cell {other:?}"),
    }
}

fn snapshot(adapter: &mut MysqlAdapter, table: &Table) -> Vec<(i64, String, i64)> {
    adapter
        .query_rows(
            &format!("SELECT id, name, amount FROM {} ORDER BY id", table.q()),
            &[],
        )
        .expect("snapshot")
        .into_iter()
        .map(|row| match row.as_slice() {
            [DatasetCell::Int(id), DatasetCell::Text(name), DatasetCell::Int(amount)] => {
                (*id, name.clone(), *amount)
            }
            other => panic!("unexpected row {other:?}"),
        })
        .collect()
}

fn int(value: i64) -> Value {
    Value::Int(value)
}

fn text(value: &str) -> Value {
    Value::Bytes(value.as_bytes().to_vec())
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn connection_reports_mysql_capabilities() {
    let mut adapter = connect();
    assert_eq!(adapter.capabilities(), MYSQL_CAPABILITIES);
    assert_eq!(adapter.test_connection(), Ok(MYSQL_CAPABILITIES));
    let declared = adapter.capabilities();
    assert!(!declared.transactional_ddl && !declared.cancellation && !declared.bulk_load);
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn missing_password_and_bad_credentials_fail_without_leaking() {
    let mut missing =
        MysqlProfile::new("127.0.0.1", 1, "u", "d", "DOWAI_MYSQL_VAR_THAT_IS_NOT_SET");
    missing.set_tls_mode(TlsMode::Disabled);
    assert_eq!(
        MysqlAdapter::connect(&missing).err(),
        Some(MysqlError::MissingPassword)
    );
    let refused = MysqlProfile::new("127.0.0.1", 1, "u", "d", "DOWAI_TEST_MYSQL_PASSWORD");
    let mut refused = refused;
    refused.set_tls_mode(TlsMode::Disabled);
    assert_eq!(
        MysqlAdapter::connect(&refused).err(),
        Some(MysqlError::ConnectionFailed)
    );
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn write_and_read_round_trip_with_parameters() {
    let mut adapter = connect();
    let table = Table::create(&mut adapter, "ct_rw", PEOPLE);
    let inserted = adapter
        .execute_sql(
            &format!(
                "INSERT INTO {} (id, name, amount) VALUES (?, ?, ?), (?, ?, ?)",
                table.q()
            ),
            &[
                int(1),
                text("Ana"),
                int(10),
                int(2),
                text("O'Brien; DROP TABLE"),
                int(20),
            ],
        )
        .expect("insert");
    assert_eq!(inserted, 2);
    assert_eq!(
        snapshot(&mut adapter, &table),
        vec![
            (1, "Ana".to_owned(), 10),
            (2, "O'Brien; DROP TABLE".to_owned(), 20)
        ]
    );
    let updated = adapter
        .execute_sql(
            &format!("UPDATE {} SET amount = amount + ? WHERE id = ?", table.q()),
            &[int(5), int(1)],
        )
        .expect("update");
    assert_eq!(updated, 1);
    let columns = adapter.introspect_table(&table.0).expect("introspect");
    assert_eq!(
        columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        ["id", "name", "amount"]
    );
    assert_eq!(columns[1].data_type, "varchar(20)");
    assert_eq!(
        adapter.introspect_table("tabela_que_nao_existe_xyz").err(),
        Some(MysqlError::QueryFailed)
    );
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn types_decode_without_loss() {
    let mut adapter = connect();
    let table = Table::create(
        &mut adapter,
        "ct_ty",
        "id INT PRIMARY KEY, d DECIMAL(10,2), dt DATE, ts DATETIME(3), f DOUBLE, big BIGINT UNSIGNED, n VARCHAR(5) NULL",
    );
    adapter
        .execute_sql(
            &format!(
                "INSERT INTO {} VALUES (1, 12.50, '2024-02-29', '2024-02-29 10:11:12.500', 1.5, 18446744073709551615, NULL)",
                table.q()
            ),
            &[],
        )
        .expect("insert");
    let rows = adapter
        .query_rows(&format!("SELECT * FROM {}", table.q()), &[])
        .expect("select");
    assert_eq!(
        rows[0],
        vec![
            DatasetCell::Int(1),
            DatasetCell::Text("12.50".to_owned()),
            DatasetCell::Text("2024-02-29".to_owned()),
            DatasetCell::Text("2024-02-29 10:11:12.500000".to_owned()),
            DatasetCell::Float(1.5),
            DatasetCell::Text("18446744073709551615".to_owned()),
            DatasetCell::Null,
        ]
    );
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn strict_mode_rejects_truncation() {
    let mut adapter = connect();
    let table = Table::create(&mut adapter, "ct_strict", PEOPLE);
    let error = adapter
        .execute_sql(
            &format!("INSERT INTO {} (id, name) VALUES (1, ?)", table.q()),
            &[text("this name is far too long for twenty")],
        )
        .err();
    assert_eq!(error, Some(MysqlError::QueryFailed));
    assert_eq!(count(&mut adapter, &table), 0);
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn transaction_commit_and_rollback() {
    let mut adapter = connect();
    let table = Table::create(&mut adapter, "ct_tx", PEOPLE);
    let insert = format!("INSERT INTO {} (id, name) VALUES (?, 'x')", table.q());

    adapter.begin_transaction().expect("begin");
    adapter.execute_sql(&insert, &[int(1)]).expect("insert");
    adapter.rollback().expect("rollback");
    assert_eq!(count(&mut adapter, &table), 0);

    adapter.begin_transaction().expect("begin");
    adapter.execute_sql(&insert, &[int(2)]).expect("insert");
    adapter.commit().expect("commit");
    assert_eq!(count(&mut adapter, &table), 1);

    // Outra conexão só enxerga o que foi confirmado.
    let mut other = connect();
    adapter.begin_transaction().expect("begin");
    adapter.execute_sql(&insert, &[int(3)]).expect("insert");
    assert_eq!(count(&mut other, &table), 1);
    adapter.rollback().expect("rollback");

    assert_eq!(adapter.commit(), Err(MysqlError::InvalidTransactionState));
    assert_eq!(adapter.rollback(), Err(MysqlError::InvalidTransactionState));
    adapter.begin_transaction().expect("begin");
    assert_eq!(
        adapter.begin_transaction(),
        Err(MysqlError::InvalidTransactionState)
    );
    adapter.rollback().expect("rollback");
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn ddl_inside_transaction_is_refused_because_mysql_commits_implicitly() {
    let mut adapter = connect();
    let table = Table::create(&mut adapter, "ct_ddl", PEOPLE);
    adapter.begin_transaction().expect("begin");
    adapter
        .execute_sql(
            &format!("INSERT INTO {} (id, name) VALUES (1, 'x')", table.q()),
            &[],
        )
        .expect("insert");
    for sql in [
        format!(
            "CREATE TABLE {} (id INT)",
            quote_identifier(&unique("ct_nope")).unwrap()
        ),
        format!("ALTER TABLE {} ADD COLUMN extra INT", table.q()),
        format!("TRUNCATE TABLE {}", table.q()),
        format!("DROP TABLE {}", table.q()),
    ] {
        assert!(
            matches!(
                adapter.execute_sql(&sql, &[]),
                Err(MysqlError::Unsupported(_))
            ),
            "{sql}"
        );
    }
    // Nada foi confirmado implicitamente: o rollback desfaz o INSERT.
    adapter.rollback().expect("rollback");
    assert_eq!(count(&mut adapter, &table), 0);
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn identifiers_are_quoted_and_validated() {
    let mut adapter = connect();
    let table = Table::create(&mut adapter, "ct_id", PEOPLE);
    // Nome com crase e vazio não viram SQL.
    assert_eq!(
        adapter.introspect_table("").err(),
        Some(MysqlError::InvalidIdentifier)
    );
    assert!(adapter.introspect_table("a`; DROP TABLE x; --").is_err());
    adapter.begin_transaction().expect("begin");
    assert_eq!(
        adapter.create_staging(&table.0, "bad\0name"),
        Err(MysqlError::InvalidIdentifier)
    );
    adapter.rollback().expect("rollback");
}

fn stream_batches(
    adapter: &mut MysqlAdapter,
    sql: &str,
    limits: StreamLimits,
) -> Result<Vec<usize>, MysqlError> {
    let mut stream = adapter.stream_query(sql, &[], limits)?;
    let mut sizes = Vec::new();
    while let Some(batch) = stream.next_batch()? {
        assert!(batch.estimated_bytes <= limits.max_inflight_bytes);
        sizes.push(batch.rows.len());
    }
    assert_eq!(stream.rows_read(), sizes.iter().sum::<usize>() as u64);
    assert_eq!(stream.next_batch().expect("after end"), None);
    stream.finish()?;
    Ok(sizes)
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn stream_delivers_bounded_batches() {
    let mut adapter = connect();
    let table = Table::create(&mut adapter, "ct_st", PEOPLE);
    let values: Vec<String> = (1..=25)
        .map(|id| format!("({id}, 'n{id}', {id})"))
        .collect();
    adapter
        .execute_sql(
            &format!(
                "INSERT INTO {} (id, name, amount) VALUES {}",
                table.q(),
                values.join(",")
            ),
            &[],
        )
        .expect("seed");
    let select = format!("SELECT id, name FROM {} ORDER BY id", table.q());
    let limits = StreamLimits {
        batch_rows: 10,
        max_inflight_bytes: 1_000_000,
    };
    assert_eq!(
        stream_batches(&mut adapter, &select, limits),
        Ok(vec![10, 10, 5])
    );

    // Teto de bytes menor que o lote: lotes menores, nenhuma linha perdida.
    let tight = StreamLimits {
        batch_rows: 1000,
        max_inflight_bytes: 130,
    };
    let sizes = stream_batches(&mut adapter, &select, tight).expect("tight stream");
    assert_eq!(sizes.iter().sum::<usize>(), 25);
    assert!(sizes.len() > 1 && sizes.iter().all(|size| *size < 25));

    // Uma única linha acima do teto falha fechado.
    let tiny = StreamLimits {
        batch_rows: 10,
        max_inflight_bytes: 8,
    };
    assert_eq!(
        stream_batches(&mut adapter, &select, tiny),
        Err(MysqlError::InflightLimitExceeded)
    );

    // Parâmetros vinculados, colunas e total desconhecido.
    let mut stream = adapter
        .stream_query(
            &format!(
                "SELECT id, name FROM {} WHERE id > ? ORDER BY id",
                table.q()
            ),
            &[int(20)],
            limits,
        )
        .expect("stream");
    assert_eq!(stream.columns()[0].0, "id");
    assert_eq!(stream.columns()[1].0, "name");
    assert_eq!(stream.total_rows(), dowai_postgres::RowCount::Unknown);
    assert_eq!(
        stream
            .next_batch()
            .expect("batch")
            .expect("rows")
            .rows
            .len(),
        5
    );
    drop(stream);
    // A conexão continua utilizável depois de descartar o stream.
    assert_eq!(count(&mut adapter, &table), 25);
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn stream_refuses_invalid_limits_and_open_transaction() {
    let mut adapter = connect();
    let ok = StreamLimits {
        batch_rows: 1,
        max_inflight_bytes: 100,
    };
    for limits in [
        StreamLimits {
            batch_rows: 0,
            ..ok
        },
        StreamLimits {
            batch_rows: 1_000_001,
            ..ok
        },
        StreamLimits {
            max_inflight_bytes: 0,
            ..ok
        },
    ] {
        assert_eq!(
            adapter.stream_query("SELECT 1", &[], limits).err(),
            Some(MysqlError::InvalidStreamLimit)
        );
    }
    adapter.begin_transaction().expect("begin");
    assert_eq!(
        adapter.stream_query("SELECT 1", &[], ok).err(),
        Some(MysqlError::InvalidTransactionState)
    );
    adapter.rollback().expect("rollback");
    assert!(adapter.stream_query("SELEC nope", &[], ok).is_err());
}

fn merge_once(
    adapter: &mut MysqlAdapter,
    table: &Table,
    rows: &[(i64, &str, i64)],
) -> Result<dowai_postgres::MergeCounters, MysqlError> {
    let columns = names(&["id", "name", "amount"]);
    adapter.begin_transaction()?;
    let result = (|| {
        adapter.create_staging(&table.0, "stage_ct")?;
        let batch: Vec<Vec<Value>> = rows
            .iter()
            .map(|(id, name, amount)| vec![int(*id), text(name), int(*amount)])
            .collect();
        adapter.write_staging_batch("stage_ct", &columns, &batch)?;
        adapter.merge("stage_ct", &table.0, &columns, &names(&["id"]))
    })();
    match result {
        Ok(counters) => {
            adapter.commit()?;
            Ok(counters)
        }
        Err(error) => {
            let _ = adapter.rollback();
            Err(error)
        }
    }
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn staging_and_merge_upsert_with_counts_and_idempotence() {
    let mut adapter = connect();
    let table = Table::create(&mut adapter, "ct_mg", PEOPLE);
    let first = merge_once(&mut adapter, &table, &[(1, "a", 1), (2, "b", 2)]).expect("merge 1");
    assert_eq!((first.inserted, first.updated), (2, 0));
    let second = merge_once(&mut adapter, &table, &[(2, "b2", 22), (3, "c", 3)]).expect("merge 2");
    assert_eq!((second.inserted, second.updated), (1, 1));
    let rerun = merge_once(&mut adapter, &table, &[(2, "b2", 22), (3, "c", 3)]).expect("rerun");
    assert_eq!((rerun.inserted, rerun.updated), (0, 2));
    assert_eq!(
        snapshot(&mut adapter, &table),
        vec![
            (1, "a".to_owned(), 1),
            (2, "b2".to_owned(), 22),
            (3, "c".to_owned(), 3)
        ]
    );
    // A staging temporária não sobrevive ao commit.
    assert!(adapter.query_rows("SELECT 1 FROM stage_ct", &[]).is_err());
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn merge_rolls_back_and_drops_staging_on_failure() {
    let mut adapter = connect();
    let table = Table::create(&mut adapter, "ct_rb", PEOPLE);
    merge_once(&mut adapter, &table, &[(1, "a", 1)]).expect("seed");
    // Chave repetida na staging.
    assert_eq!(
        merge_once(&mut adapter, &table, &[(5, "x", 1), (5, "y", 2)]).err(),
        Some(MysqlError::DuplicateMergeKey)
    );
    // Valor longo demais na segunda linha: a primeira também é desfeita.
    let too_long = "z".repeat(40);
    assert_eq!(
        merge_once(
            &mut adapter,
            &table,
            &[(6, "ok", 1), (7, too_long.as_str(), 2)]
        )
        .err(),
        Some(MysqlError::QueryFailed)
    );
    assert_eq!(snapshot(&mut adapter, &table), vec![(1, "a".to_owned(), 1)]);
    assert!(adapter.query_rows("SELECT 1 FROM stage_ct", &[]).is_err());

    // Rollback explícito depois de um merge bem-sucedido desfaz o destino.
    let columns = names(&["id", "name", "amount"]);
    adapter.begin_transaction().expect("begin");
    adapter
        .create_staging(&table.0, "stage_ct")
        .expect("staging");
    adapter
        .write_staging_batch("stage_ct", &columns, &[vec![int(9), text("n"), int(9)]])
        .expect("write");
    adapter
        .merge("stage_ct", &table.0, &columns, &names(&["id"]))
        .expect("merge");
    adapter.rollback().expect("rollback");
    assert_eq!(snapshot(&mut adapter, &table), vec![(1, "a".to_owned(), 1)]);
    assert!(adapter.query_rows("SELECT 1 FROM stage_ct", &[]).is_err());
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn write_operations_require_transaction_and_valid_specs() {
    let mut adapter = connect();
    let table = Table::create(&mut adapter, "ct_sp", PEOPLE);
    let columns = names(&["id", "name"]);
    assert_eq!(
        adapter.create_staging(&table.0, "s"),
        Err(MysqlError::InvalidTransactionState)
    );
    assert_eq!(
        adapter.write_staging_batch("s", &columns, &[]),
        Err(MysqlError::InvalidTransactionState)
    );
    assert_eq!(
        adapter.merge("s", &table.0, &columns, &names(&["id"])),
        Err(MysqlError::InvalidTransactionState)
    );
    assert_eq!(
        adapter.delete_all(&table.0),
        Err(MysqlError::InvalidTransactionState)
    );
    adapter.begin_transaction().expect("begin");
    adapter.create_staging(&table.0, "s").expect("staging");
    assert!(matches!(
        adapter.write_staging_batch("s", &columns, &[vec![int(1)]]),
        Err(MysqlError::InvalidWriteSpec(_))
    ));
    assert!(matches!(
        adapter.write_staging_batch("s", &[], &[]),
        Err(MysqlError::InvalidWriteSpec(_))
    ));
    assert_eq!(adapter.write_staging_batch("s", &columns, &[]), Ok(0));
    assert!(matches!(
        adapter.merge("s", &table.0, &columns, &[]),
        Err(MysqlError::InvalidWriteSpec(_))
    ));
    assert!(matches!(
        adapter.merge("s", &table.0, &columns, &names(&["amount"])),
        Err(MysqlError::InvalidWriteSpec(_))
    ));
    adapter.rollback().expect("rollback");
}

#[test]
#[ignore = "opt-in contract test; requires a disposable MySQL database"]
fn append_replace_and_unique_key_detection() {
    let mut adapter = connect();
    let table = Table::create(
        &mut adapter,
        "ct_ap",
        "id INT PRIMARY KEY, name VARCHAR(20) NOT NULL, amount INT NOT NULL DEFAULT 0, UNIQUE KEY u_name (name, amount)",
    );
    assert_eq!(adapter.has_unique_key(&table.0, &names(&["id"])), Ok(true));
    assert_eq!(
        adapter.has_unique_key(&table.0, &names(&["amount", "name"])),
        Ok(true)
    );
    assert_eq!(
        adapter.has_unique_key(&table.0, &names(&["name"])),
        Ok(false)
    );
    assert_eq!(
        adapter.has_unique_key(&table.0, &names(&["id", "name"])),
        Ok(false)
    );

    let columns = names(&["id", "name"]);
    adapter.begin_transaction().expect("begin");
    adapter.create_staging(&table.0, "s").expect("staging");
    adapter
        .write_staging_batch(
            "s",
            &columns,
            &[vec![int(1), text("a")], vec![int(2), text("b")]],
        )
        .expect("stage");
    assert_eq!(adapter.append_staging("s", &table.0, &columns), Ok(2));
    adapter.commit().expect("commit");
    assert_eq!(count(&mut adapter, &table), 2);

    // Replace: DELETE + append na mesma transação; rollback restaura as linhas antigas.
    adapter.begin_transaction().expect("begin");
    adapter.create_staging(&table.0, "s").expect("staging");
    adapter
        .write_staging_batch("s", &columns, &[vec![int(9), text("z")]])
        .expect("stage");
    assert_eq!(adapter.delete_all(&table.0), Ok(2));
    assert_eq!(adapter.append_staging("s", &table.0, &columns), Ok(1));
    adapter.rollback().expect("rollback");
    assert_eq!(count(&mut adapter, &table), 2);
}
