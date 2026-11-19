use dowai_databases_oracle::{
    AdapterError, ConnectionProfile, OracleAdapter, TableIdentifier, TlsMode,
};
use serde_json::json;
use std::env;

fn profile() -> ConnectionProfile {
    let port = env::var("DOWAI_TEST_ORACLE_PORT")
        .expect("Oracle test port is required")
        .parse()
        .expect("Oracle test port must be a number");
    let mode = match env::var("DOWAI_TEST_ORACLE_TRANSPORT").as_deref() {
        Ok("native") => TlsMode::NativeRequired,
        Ok("tcps") | Err(env::VarError::NotPresent) => TlsMode::Required,
        _ => panic!("Oracle test transport must be 'native' or 'tcps'"),
    };
    ConnectionProfile::new(
        env::var("DOWAI_TEST_ORACLE_USER").expect("Oracle test user is required"),
        env::var("DOWAI_TEST_ORACLE_HOST").expect("Oracle test host is required"),
        port,
        env::var("DOWAI_TEST_ORACLE_SERVICE").expect("Oracle test service is required"),
        "DOWAI_ORACLE_PASSWORD",
    )
    .with_tls_mode(mode)
    .with_connect_timeout_secs(10)
}

#[test]
#[ignore = "requires Oracle Client, encrypted transport, test credentials and CREATE TABLE privilege"]
fn live_query_and_rollback() {
    let adapter = match OracleAdapter::connect(&profile()) {
        Ok(adapter) => adapter,
        Err(error) => {
            let message = error.to_string();
            if message.contains("DPI-1047") {
                panic!("Oracle Client library could not be loaded (DPI-1047)");
            }

            for (prefix, length) in [("ORA-", 9), ("DPI-", 8)] {
                if let Some(start) = message.find(prefix) {
                    if let Some(code) = message.get(start..start + length) {
                        if code[4..].bytes().all(|byte| byte.is_ascii_digit()) {
                            panic!("Oracle connection failed ({code})");
                        }
                    }
                }
            }
            panic!("Oracle connection failed; check Oracle Client and TCPS configuration");
        }
    };
    assert!(adapter.test_connection().is_ok(), "connection test failed");
    if env::var("DOWAI_TEST_ORACLE_TRANSPORT").as_deref() == Ok("native") {
        let banners = adapter
            .query_json(
                "SELECT NETWORK_SERVICE_BANNER FROM V$SESSION_CONNECT_INFO \
                 WHERE SID = SYS_CONTEXT('USERENV', 'SID')",
                &[],
            )
            .unwrap_or_else(|_| panic!("cannot verify Oracle session encryption"));
        assert!(
            banners.iter().any(|row| row["NETWORK_SERVICE_BANNER"]
                .as_str()
                .is_some_and(|banner| banner.to_ascii_uppercase().contains("AES"))),
            "Oracle session did not report AES encryption"
        );
        assert!(matches!(
            OracleAdapter::connect(&profile().with_tls_mode(TlsMode::Required)),
            Err(AdapterError::ClientConfigurationUnavailable)
        ));
    }
    let table = format!(
        "DOWAI_TEST_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..16]
    );
    let relation = format!("\"{table}\"");
    assert!(
        adapter
            .execute(
                &format!(
                    "CREATE TABLE {relation} (\"ID\" NUMBER PRIMARY KEY, \"LABEL\" VARCHAR2(40))"
                ),
                &[],
            )
            .is_ok(),
        "Oracle test table creation failed"
    );
    let result = (|| {
        adapter
            .write_rows(
                &TableIdentifier::new(&table),
                &[json!({"ID": 7, "LABEL": "before"})],
            )
            .map_err(|_| "write failed")?;
        adapter
            .upsert_rows(
                &TableIdentifier::new(&table),
                &[json!({"ID": 7, "LABEL": "after"})],
                &["ID"],
            )
            .map_err(|_| "upsert failed")?;
        let rows = adapter
            .query_json(&format!("SELECT \"ID\", \"LABEL\" FROM {relation}"), &[])
            .map_err(|_| "query failed")?;
        if rows != vec![json!({"ID": "7", "LABEL": "after"})] {
            return Err("unexpected query result");
        }
        adapter.rollback().map_err(|_| "rollback failed")?;
        let rows = adapter
            .query_json(&format!("SELECT \"ID\" FROM {relation}"), &[])
            .map_err(|_| "query after rollback failed")?;
        if !rows.is_empty() {
            return Err("rollback did not discard rows");
        }
        Ok(())
    })();
    let cleanup = adapter.execute(&format!("DROP TABLE {relation} PURGE"), &[]);
    assert!(cleanup.is_ok(), "Oracle test table cleanup failed");
    assert!(
        result.is_ok(),
        "Oracle query or write failed: {}",
        result.unwrap_err()
    );
}

#[test]
#[ignore = "requires Oracle Client, encrypted transport, test credentials and CREATE privileges"]
fn live_sql_materialization() {
    let adapter =
        OracleAdapter::connect(&profile()).unwrap_or_else(|_| panic!("Oracle connection failed"));
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let table = TableIdentifier::new(format!("DOWAI_T_{}", &suffix[..16]));
    let view = TableIdentifier::new(format!("DOWAI_V_{}", &suffix[..16]));
    let table_name = format!("\"{}\"", table.name);
    let view_name = format!("\"{}\"", view.name);
    let mut table_created = false;
    let mut view_created = false;
    let result = (|| {
        adapter
            .materialize_table(&table, "SELECT 7 AS \"ID\" FROM DUAL")
            .map_err(|_| "table materialization failed")?;
        table_created = true;
        adapter
            .materialize_view(&view, &format!("SELECT \"ID\" FROM {table_name}"))
            .map_err(|_| "view materialization failed")?;
        view_created = true;
        let rows = adapter
            .query_json(&format!("SELECT \"ID\" FROM {view_name}"), &[])
            .map_err(|_| "view query failed")?;
        if rows != vec![json!({"ID": "7"})] {
            return Err("unexpected materialization result");
        }

        Ok(())
    })();
    let view_cleanup = !view_created
        || adapter
            .execute(&format!("DROP VIEW {view_name}"), &[])
            .is_ok();
    let table_cleanup = !table_created
        || adapter
            .execute(&format!("DROP TABLE {table_name} PURGE"), &[])
            .is_ok();
    assert!(
        view_cleanup && table_cleanup,
        "Oracle materialization cleanup failed"
    );
    assert!(
        result.is_ok(),
        "Oracle materialization failed: {}",
        result.unwrap_err()
    );
}

#[test]
#[ignore = "requires Oracle Client, encrypted transport and DBMS_SESSION execution"]
fn live_round_trip_timeout() {
    let adapter = OracleAdapter::connect(&profile().with_call_timeout_secs(1))
        .unwrap_or_else(|_| panic!("Oracle connection failed"));
    let start = std::time::Instant::now();
    let result = adapter.execute("BEGIN DBMS_SESSION.SLEEP(3); END;", &[]);
    let elapsed = start.elapsed();
    let error = result
        .expect_err("the delayed call must time out")
        .to_string();
    assert!(
        error.contains("DPI-1067") || error.contains("ORA-03156") || error.contains("ORA-03114"),
        "Oracle did not report a call timeout"
    );
    assert!(
        elapsed >= std::time::Duration::from_millis(500)
            && elapsed < std::time::Duration::from_secs(5),
        "Oracle timeout was outside the expected range"
    );
}
