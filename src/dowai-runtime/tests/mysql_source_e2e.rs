//! E2E de `env.source` com perfil MySQL (ENT-116, D-121).
//!
//! Opt-in (`--ignored --test-threads=1`: o estado de sessão usa variáveis de ambiente do
//! processo): exige `DOWAI_TEST_MYSQL_*` (MySQL descartável) e um `python3` capaz de criar venv.
//! Cada teste cria tabelas com nome único e as remove ao fim. O perfil `warehouse` (PostgreSQL)
//! do projeto nunca é aberto por estes testes: as fontes MySQL não o utilizam.

use dowai_core::{create_project, default_session_store_path, DowaiError, SessionStore};
use dowai_mysql::{quote_identifier as my_ident, MysqlAdapter, MysqlProfile};
use dowai_postgres::TlsMode;
use dowai_runtime::{run_postgres, RunRequest, RunResult};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is required"))
}

fn mysql_profile() -> MysqlProfile {
    let mut profile = MysqlProfile::new(
        env_var("DOWAI_TEST_MYSQL_HOST"),
        env_var("DOWAI_TEST_MYSQL_PORT").parse().expect("port"),
        env_var("DOWAI_TEST_MYSQL_USER"),
        env_var("DOWAI_TEST_MYSQL_DATABASE"),
        "DOWAI_TEST_MYSQL_PASSWORD",
    );
    if std::env::var("DOWAI_TEST_MYSQL_TLS").as_deref() != Ok("required") {
        profile.set_tls_mode(TlsMode::Disabled);
    }
    profile
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos()
}

struct EnvironmentRestore {
    name: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentRestore {
    fn set(name: &'static str, value: &Path) -> Self {
        let previous = std::env::var_os(name);
        std::env::set_var(name, value);
        Self { name, previous }
    }
}

impl Drop for EnvironmentRestore {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => std::env::set_var(self.name, value),
            None => std::env::remove_var(self.name),
        }
    }
}

/// Remove as tabelas MySQL criadas, mesmo se uma asserção falhar.
struct TableCleanup(Vec<String>);

impl Drop for TableCleanup {
    fn drop(&mut self) {
        if let Ok(mut adapter) = MysqlAdapter::connect(&mysql_profile()) {
            for table in &self.0 {
                let _ = adapter.execute_sql(
                    &format!(
                        "DROP TABLE IF EXISTS {}",
                        my_ident(table).expect("identifier")
                    ),
                    &[],
                );
            }
        }
    }
}

struct Fixture {
    project: PathBuf,
    _env: Vec<EnvironmentRestore>,
}

impl Fixture {
    /// Projeto com venv Python, escrita externa permitida (o run Python-only a exige) e a fonte
    /// `mart.items` apontando para `table` no perfil MySQL `mart`.
    fn new(test: &str, table: &str, extra_sources: Value) -> Self {
        let root = std::env::temp_dir().join(format!(
            "dowai-mysql-source-e2e-{test}-{}-{}",
            std::process::id(),
            nonce()
        ));
        let state = root.join("state");
        for dir in ["local", "roaming", "system"] {
            fs::create_dir_all(state.join(dir)).expect("create state dir");
        }
        let env = vec![
            EnvironmentRestore::set("LOCALAPPDATA", &state.join("local")),
            EnvironmentRestore::set("APPDATA", &state.join("roaming")),
            EnvironmentRestore::set("PROGRAMDATA", &state.join("system")),
            EnvironmentRestore::set("XDG_CONFIG_HOME", &state.join("roaming")),
            EnvironmentRestore::set("XDG_STATE_HOME", &state),
        ];
        let project = create_project(&root, "fixture").expect("initialize project");
        let venv = project.join(".venv");
        let created = Command::new("python3")
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .output()
            .expect("create virtual environment");
        assert!(created.status.success(), "venv creation failed");

        let manifest_path = project.join("dowai_project.json");
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read manifest"))
                .expect("manifest is JSON");
        manifest["python"] = json!({
            "interpreter": venv.join("bin").join("python"),
            "timeout_seconds": 60
        });
        manifest["config"]["security"]["allow_external_writes"] = json!(true);
        let mut sources = json!({
            "mart": {
                "schema": env_var("DOWAI_TEST_MYSQL_DATABASE"),
                "tables": { "items": table },
                "profile": "mart"
            }
        });
        if let (Some(target), Some(extra)) = (sources.as_object_mut(), extra_sources.as_object()) {
            for (name, value) in extra {
                target.insert(name.clone(), value.clone());
            }
        }
        manifest["sources"] = sources;
        fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("json"))
            .expect("write manifest");

        // The PostgreSQL profile is never opened by MySQL sources; placeholders keep the
        // workspace valid when the PostgreSQL test variables are absent.
        let workspace = json!({
            "schema_version": 2,
            "profiles": {
                "warehouse": {
                    "type": "postgres",
                    "host": std::env::var("DOWAI_TEST_PG_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
                    "port": std::env::var("DOWAI_TEST_PG_PORT").ok().and_then(|p| p.parse::<u16>().ok()).unwrap_or(5432),
                    "user": std::env::var("DOWAI_TEST_PG_USER").unwrap_or_else(|_| "unused".into()),
                    "database": std::env::var("DOWAI_TEST_PG_DATABASE").unwrap_or_else(|_| "unused".into()),
                    "password_env": "DOWAI_TEST_PG_PASSWORD",
                    "tls": "disabled"
                },
                "mart": {
                    "type": "mysql",
                    "host": env_var("DOWAI_TEST_MYSQL_HOST"),
                    "port": env_var("DOWAI_TEST_MYSQL_PORT").parse::<u16>().expect("port"),
                    "user": env_var("DOWAI_TEST_MYSQL_USER"),
                    "database": env_var("DOWAI_TEST_MYSQL_DATABASE"),
                    "password_env": "DOWAI_TEST_MYSQL_PASSWORD",
                    "tls": if std::env::var("DOWAI_TEST_MYSQL_TLS").as_deref() == Ok("required") { "required" } else { "disabled" }
                }
            },
            "defaults": { "execution": { "default_profile": "warehouse" } }
        });
        fs::write(
            root.join("workspace.json"),
            serde_json::to_vec(&workspace).expect("json"),
        )
        .expect("write workspace");
        Self { project, _env: env }
    }

    fn write_model(&self, file: &str, body: &str) {
        fs::write(self.project.join("models").join(file), body).expect("write model");
    }

    fn run(&self, target: &str) -> Result<RunResult, DowaiError> {
        run_postgres(
            &self.project,
            RunRequest {
                targets: vec![target.to_owned()],
                mode: None,
                profile: None,
                session_id: None,
                variables: BTreeMap::new(),
                confirm_destructive: false,
                full_refresh: false,
            },
        )
    }

    fn variables(&self, result: &RunResult) -> BTreeMap<String, Value> {
        SessionStore::for_user_state(default_session_store_path().expect("state path"))
            .snapshot(&result.session_id)
            .expect("session snapshot")
    }
}

fn mysql_exec(sql: &str) {
    MysqlAdapter::connect(&mysql_profile())
        .expect("connect MySQL")
        .execute_sql(sql, &[])
        .unwrap_or_else(|error| panic!("MySQL statement failed: {error:?}"));
}

/// ENT-116: `env.source` over a `mysql` profile returns the schema, the rows in primary-key
/// order and a projection; a filter is refused with a stable code; the project is not touched.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable MySQL database"]
fn ent116_mysql_source_reads_schema_ordered_batches_and_projection() {
    let table = format!("dowai_ent116_items_{}", nonce());
    let _cleanup = TableCleanup(vec![table.clone()]);
    mysql_exec(&format!(
        "CREATE TABLE {table} (id INT PRIMARY KEY, label VARCHAR(40) NOT NULL)"
    ));
    mysql_exec(&format!(
        "INSERT INTO {table} VALUES (3, 'c'), (1, 'a'), (2, 'b')"
    ));
    let fx = Fixture::new("read", &table, json!({}));
    fx.write_model(
        "reader.py",
        "def model(env):\n\
         \x20   src = env.source('mart', 'items')\n\
         \x20   columns = src.schema()\n\
         \x20   env.setVar('cols', ','.join(c['name'] for c in columns))\n\
         \x20   rows = list(src.iter_rows(batch_size=2))\n\
         \x20   env.setVar('n', len(rows))\n\
         \x20   env.setVar('id_sum', sum(r['id'] for r in rows))\n\
         \x20   env.setVar('labels', ','.join(r['label'] for r in rows))\n\
         \x20   picked = list(src.select('label').iter_rows(batch_size=10))\n\
         \x20   env.setVar('picked_keys', ','.join(sorted(picked[0].keys())))\n\
         \x20   try:\n\
         \x20       list(src.filter('id', '=', 1).iter_rows(batch_size=10))\n\
         \x20   except Exception as error:\n\
         \x20       env.setVar('filter_code', error.code)\n\
         \x20   return None\n",
    );

    let result = fx.run("reader.py").expect("mysql source run");

    assert_eq!(result.status, "SUCCEEDED");
    let variables = fx.variables(&result);
    assert_eq!(variables["cols"], json!("id,label"));
    assert_eq!(variables["n"], json!(3));
    assert_eq!(variables["id_sum"], json!(6));
    assert_eq!(variables["labels"], json!("a,b,c"));
    assert_eq!(variables["picked_keys"], json!("label"));
    assert_eq!(
        variables["filter_code"],
        json!("DOWAI_DATASET_FILTER_UNSUPPORTED")
    );
}

/// ENT-116: a MySQL source table without a primary key is refused: batches need a stable order.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable MySQL database"]
fn ent116_mysql_source_without_primary_key_is_refused_with_stable_code() {
    let table = format!("dowai_ent116_nokey_{}", nonce());
    let _cleanup = TableCleanup(vec![table.clone()]);
    mysql_exec(&format!("CREATE TABLE {table} (id INT, label VARCHAR(40))"));
    mysql_exec(&format!("INSERT INTO {table} VALUES (1, 'a')"));
    let fx = Fixture::new("nokey", &table, json!({}));
    fx.write_model(
        "reader.py",
        "def model(env):\n\
         \x20   try:\n\
         \x20       list(env.source('mart', 'items').iter_rows(batch_size=10))\n\
         \x20   except Exception as error:\n\
         \x20       env.setVar('code', error.code)\n\
         \x20   return None\n",
    );

    let result = fx.run("reader.py").expect("run");

    assert_eq!(result.status, "SUCCEEDED");
    let variables = fx.variables(&result);
    assert_eq!(variables["code"], json!("DOWAI_DATASET_UNAVAILABLE"));
}

/// ENT-116: a source whose profile is not a MySQL profile (here, a name that does not exist)
/// fails only when read, with `DOWAI_DATASET_UNAVAILABLE`; the run itself is not blocked.
#[test]
#[ignore = "opt-in E2E; requires a dedicated disposable MySQL database"]
fn ent116_source_with_missing_profile_fails_only_on_read() {
    let table = format!("dowai_ent116_missing_{}", nonce());
    let fx = Fixture::new(
        "missing",
        &table,
        json!({
            "broken": {
                "schema": env_var("DOWAI_TEST_MYSQL_DATABASE"),
                "tables": { "items": table },
                "profile": "does_not_exist"
            }
        }),
    );
    fx.write_model(
        "reader.py",
        "def model(env):\n\
         \x20   try:\n\
         \x20       env.source('broken', 'items').schema()\n\
         \x20   except Exception as error:\n\
         \x20       env.setVar('code', error.code)\n\
         \x20   return None\n",
    );

    let result = fx.run("reader.py").expect("run");

    assert_eq!(result.status, "SUCCEEDED");
    let variables = fx.variables(&result);
    assert_eq!(variables["code"], json!("DOWAI_DATASET_UNAVAILABLE"));
}
