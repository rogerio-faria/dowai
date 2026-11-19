use dowai_sqlserver::{JsonRecord, Materialization, SqlServerAdapter, SqlServerProfile};
use serde_json::json;
use std::env;

fn profile() -> SqlServerProfile {
    let port = env::var("DOWAI_TEST_SQLSERVER_PORT")
        .expect("SQL Server test port is required")
        .parse()
        .expect("SQL Server test port must be a number");
    let mut profile = SqlServerProfile::new(
        env::var("DOWAI_TEST_SQLSERVER_HOST").expect("SQL Server test host is required"),
        port,
        env::var("DOWAI_TEST_SQLSERVER_DATABASE").expect("SQL Server test database is required"),
        env::var("DOWAI_TEST_SQLSERVER_USER").expect("SQL Server test user is required"),
        "DOWAI_SQLSERVER_PASSWORD",
    );
    profile.connect_timeout = std::time::Duration::from_secs(30);
    profile
}

#[tokio::test]
#[ignore = "requires a TLS-configured SQL Server and test credentials"]
async fn live_query_and_rollback() {
    let mut adapter = match SqlServerAdapter::connect(&profile()).await {
        Ok(adapter) => adapter,
        Err(error) => panic!("SQL Server connection failed: {error}"),
    };
    assert!(
        adapter.test_connection().await.is_ok(),
        "connection test failed"
    );
    let table = format!("dowai_test_{}", uuid::Uuid::new_v4().simple());
    if let Err(error) = adapter.begin_transaction().await {
        panic!("SQL Server begin failed: {error}");
    }

    let result = async {
        adapter
            .execute_sql(&format!(
                "CREATE TABLE [{table}] ([id] int NOT NULL PRIMARY KEY, [label] nvarchar(40))"
            ))
            .await?;
        adapter
            .execute_json(
                &format!("INSERT INTO [{table}] ([id], [label]) VALUES (@P1, @P2)"),
                &[json!(7), json!("before")],
            )
            .await?;
        let update: JsonRecord = [
            ("id".to_owned(), json!(7)),
            ("label".to_owned(), json!("after")),
        ]
        .into_iter()
        .collect();
        adapter
            .upsert_rows(&table, &[update], &["id".to_owned()])
            .await?;
        let rows = adapter
            .query_json_rows(&format!("SELECT [id], [label] FROM [{table}]"), &[])
            .await?;
        if rows.len() != 1
            || rows[0].get("id") != Some(&json!("7"))
            || rows[0].get("label") != Some(&json!("after"))
        {
            return Err(dowai_sqlserver::AdapterError::QueryFailed);
        }
        Ok::<(), dowai_sqlserver::AdapterError>(())
    }
    .await;
    let rollback = adapter.rollback_transaction().await;
    assert!(result.is_ok(), "SQL Server query or write failed");
    assert!(rollback.is_ok(), "SQL Server rollback failed");
    let remaining = adapter
        .query_json_rows("SELECT OBJECT_ID(@P1, 'U') AS object_id", &[json!(table)])
        .await;
    assert!(remaining.is_ok(), "SQL Server cleanup verification failed");
    assert_eq!(remaining.unwrap()[0].get("object_id"), Some(&json!(null)));
}

#[tokio::test]
#[ignore = "requires a TLS-configured SQL Server and test credentials"]
async fn live_sql_materialization_rolls_back() {
    let mut adapter = SqlServerAdapter::connect(&profile())
        .await
        .unwrap_or_else(|_| panic!("SQL Server connection failed"));
    let suffix = uuid::Uuid::new_v4().simple();
    let table = format!("dowai_table_{suffix}");
    let view = format!("dowai_view_{suffix}");
    adapter
        .begin_transaction()
        .await
        .unwrap_or_else(|_| panic!("SQL Server begin failed"));
    let result = async {
        adapter
            .materialize_sql(&table, Materialization::Table, "SELECT 7 AS id", false)
            .await?;
        adapter
            .materialize_sql(
                &view,
                Materialization::View,
                &format!("SELECT [id] FROM [{table}]"),
                false,
            )
            .await?;
        let rows = adapter
            .query_json_rows(&format!("SELECT [id] FROM [{view}]"), &[])
            .await?;
        if rows.len() != 1 || rows[0].get("id") != Some(&json!("7")) {
            return Err(dowai_sqlserver::AdapterError::QueryFailed);
        }
        Ok::<(), dowai_sqlserver::AdapterError>(())
    }
    .await;
    let rollback = adapter.rollback_transaction().await;
    assert!(rollback.is_ok(), "SQL Server rollback failed");
    assert!(result.is_ok(), "SQL Server materialization failed");
}
