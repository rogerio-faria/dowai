use dowai_core::{
    create_project, load_project, plan, validate_plan_templates, validate_project, DowaiError,
    ExecutionMode, TemplateOptions,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct TestProject(PathBuf);

impl TestProject {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("dowai-test-{}-{nonce}", std::process::id()));
        fs::create_dir_all(path.join("models")).expect("create models directory");
        fs::write(
            path.join("dowai_project.json"),
            r#"{"schema_version":2,"name":"fixture","models_dir":"models"}"#,
        )
        .expect("write project configuration");
        Self(path)
    }

    fn write_model(&self, name: &str, body: &str) {
        let path = self.0.join("models").join(name);
        fs::create_dir_all(path.parent().expect("model has parent")).expect("create model folder");
        fs::write(path, body).expect("write model");
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn automatic_plan_runs_static_sql_dependencies_before_the_target() {
    let project = TestProject::new();
    project.write_model("source.sql", "select 1 as id");
    project.write_model("daily.sql", "select * from {{ ref('source') }}");

    let plan = plan(
        project.path(),
        &["daily.sql".into()],
        ExecutionMode::Automatic,
    )
    .expect("plan should succeed");

    assert_eq!(
        plan.nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        ["source.sql", "daily.sql"]
    );
    assert_eq!(plan.nodes[1].depends_on, ["source.sql"]);
}

#[test]
fn manual_plan_preserves_requested_order_and_reports_ignored_dependencies() {
    let project = TestProject::new();
    project.write_model("source.sql", "select 1 as id");
    project.write_model("daily.sql", "select * from {{ ref('source') }}");

    let plan = plan(
        project.path(),
        &["daily.sql".into(), "source.sql".into()],
        ExecutionMode::Manual,
    )
    .expect("manual plan should succeed");

    assert_eq!(
        plan.nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        ["daily.sql", "source.sql"]
    );
    assert!(plan
        .warnings
        .iter()
        .any(|warning| warning.contains("source.sql")));
}

#[test]
fn automatic_plan_rejects_cycles_before_returning_a_plan() {
    let project = TestProject::new();
    project.write_model("a.sql", "select * from {{ ref('b') }}");
    project.write_model("b.sql", "select * from {{ ref('a') }}");

    let error = plan(project.path(), &["a.sql".into()], ExecutionMode::Automatic)
        .expect_err("cyclic references must fail");

    assert_eq!(error.code(), "DOWAI_PLAN_CYCLE", "{error}");
}

#[test]
fn planner_rejects_a_model_path_outside_the_project() {
    let project = TestProject::new();
    let outside = std::env::temp_dir().join("outside-dowai-model.sql");
    fs::write(&outside, "select 1").expect("write outside file");

    let error = plan(
        project.path(),
        &[outside.to_string_lossy().into_owned()],
        ExecutionMode::Automatic,
    )
    .expect_err("external model path must be rejected");

    let _ = fs::remove_file(outside);
    assert_eq!(error.code(), "DOWAI_POLICY_PATH");
}

#[test]
fn project_configuration_rejects_unknown_fields() {
    let project = TestProject::new();
    fs::write(
        project.path().join("dowai_project.json"),
        r#"{"schema_version":2,"name":"fixture","models_dir":"models","unknown":true}"#,
    )
    .expect("replace config");

    let error = plan(project.path(), &["all".into()], ExecutionMode::Automatic)
        .expect_err("unknown project fields must not be silently ignored");

    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
}

#[test]
fn project_configuration_rejects_models_directory_path_traversal() {
    let project = TestProject::new();
    fs::write(
        project.path().join("dowai_project.json"),
        r#"{"schema_version":2,"name":"fixture","models_dir":"../outside"}"#,
    )
    .expect("replace config");

    let error = plan(project.path(), &["all".into()], ExecutionMode::Automatic)
        .expect_err("models directory traversal must fail");

    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
}

#[test]
fn validate_rejects_unknown_project_configuration_settings() {
    let project = TestProject::new();
    fs::write(
        project.path().join("dowai_project.json"),
        r#"{"schema_version":2,"name":"fixture","models_dir":"models","config":{"execution":{"unknown_setting":true}}}"#,
    )
    .expect("replace config");

    let report = validate_project(project.path());

    assert!(!report.valid);
    assert!(report
        .diagnostics
        .iter()
        .any(|diagnostic| diagnostic.code == "DOWAI_CONFIG_INVALID"));
}

#[test]
fn comments_and_string_literals_do_not_create_dependencies() {
    let project = TestProject::new();
    project.write_model(
        "daily.sql",
        "-- {{ ref('missing') }}\nselect 'ref(\\'not_a_call\\')' as note",
    );

    let plan = plan(
        project.path(),
        &["daily.sql".into()],
        ExecutionMode::Automatic,
    )
    .expect("references inside comments and strings are not executable");

    assert_eq!(plan.nodes.len(), 1);
    assert!(plan.nodes[0].depends_on.is_empty());
}

#[test]
fn python_literal_refs_are_included_in_the_static_plan() {
    let project = TestProject::new();
    project.write_model("source.sql", "select 1 as id");
    project.write_model(
        "consumer.py",
        "def model(env):\n    return env.ref(\"source\")\n",
    );

    let plan = plan(
        project.path(),
        &["consumer.py".into()],
        ExecutionMode::Automatic,
    )
    .expect("literal Python refs should be discovered without executing Python");

    assert_eq!(
        plan.nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        ["source.sql", "consumer.py"]
    );
}

#[test]
fn python_depends_on_declarations_are_static_dependencies() {
    let project = TestProject::new();
    project.write_model("source.sql", "select 1 as id");
    project.write_model(
        "consumer.py",
        "def model(env):\n    env.depends_on(\"source\")\n",
    );

    let plan = plan(
        project.path(),
        &["consumer.py".into()],
        ExecutionMode::Automatic,
    )
    .expect("literal depends_on declarations should be planned");

    assert_eq!(
        plan.nodes
            .iter()
            .map(|node| node.id.as_str())
            .collect::<Vec<_>>(),
        ["source.sql", "consumer.py"]
    );
}

#[test]
fn imperative_python_run_calls_do_not_create_dag_dependencies() {
    let project = TestProject::new();
    project.write_model("called.sql", "select 1 as id");
    project.write_model(
        "caller.py",
        "def model(env):\n    env.run(\"called.sql\")\n",
    );

    let plan = plan(
        project.path(),
        &["caller.py".into()],
        ExecutionMode::Automatic,
    )
    .expect("imperative calls are not static data dependencies");

    assert_eq!(plan.nodes.len(), 1);
    assert_eq!(plan.nodes[0].id, "caller.py");
    assert!(plan.nodes[0].depends_on.is_empty());
}

#[test]
fn python_methods_on_other_objects_do_not_create_env_dependencies() {
    let project = TestProject::new();
    project.write_model(
        "caller.py",
        "def model(env):\n    helper.ref('not_a_model')\n    helper.depends_on(dynamic_name)\n    return None\n",
    );

    let plan = plan(
        project.path(),
        &["caller.py".into()],
        ExecutionMode::Automatic,
    )
    .expect("unrelated object methods are not DOWAI dependencies");

    assert_eq!(plan.nodes.len(), 1);
    assert!(plan.nodes[0].depends_on.is_empty());
}

#[test]
fn python_env_reference_must_use_a_string_literal() {
    let project = TestProject::new();
    project.write_model(
        "caller.py",
        "def model(env):\n    env.depends_on(dynamic_name)\n",
    );

    let error = plan(
        project.path(),
        &["caller.py".into()],
        ExecutionMode::Automatic,
    )
    .expect_err("dynamic environment dependencies are not statically plannable");

    assert_eq!(error.code(), "DOWAI_PLAN_INVALID");
}

#[test]
fn planning_validates_templates_in_order_without_mutating_input_variables() {
    let project = TestProject::new();
    project.write_model("source.sql", "{{ def_var('enabled', true) }}select 1 as id");
    project.write_model(
        "daily.sql",
        "{% if var('enabled') %}select * from {{ ref('source') }}{% endif %}",
    );
    let execution_plan = plan(
        project.path(),
        &["daily.sql".into()],
        ExecutionMode::Automatic,
    )
    .expect("static dependency plan should succeed");
    let variables = BTreeMap::from([("provided".to_owned(), json!("value"))]);

    validate_plan_templates(
        project.path(),
        &execution_plan,
        &variables,
        &TemplateOptions::default(),
    )
    .expect("templates should see definitions from prior models");

    assert_eq!(
        variables,
        BTreeMap::from([("provided".to_owned(), json!("value"))])
    );
}

#[test]
fn planning_rejects_undefined_template_variables() {
    let project = TestProject::new();
    project.write_model("model.sql", "{% if var('missing') %}select 1{% endif %}");
    let execution_plan = plan(
        project.path(),
        &["model.sql".into()],
        ExecutionMode::Automatic,
    )
    .expect("model dependency plan should succeed");

    let error = validate_plan_templates(
        project.path(),
        &execution_plan,
        &BTreeMap::new(),
        &TemplateOptions::default(),
    )
    .expect_err("strict undefined variables should fail planning");

    assert_eq!(error.code(), "DOWAI_TEMPLATE_INVALID");
}

#[test]
fn project_initialization_creates_a_valid_empty_project() {
    let workspace = TestProject::new();

    let root = create_project(workspace.path(), "sample").expect("initialize project");
    let report = validate_project(&root);

    assert!(root.join("dowai_project.json").is_file());
    assert!(root.join("models").is_dir());
    assert!(workspace.path().join("workspace.json").is_file());
    assert!(report.valid);
}

#[test]
fn project_loads_declared_source_schema_and_table_mappings() {
    let project = TestProject::new();
    fs::write(
        project.path().join("dowai_project.json"),
        r#"{
          "schema_version": 2,
          "name": "fixture",
          "models_dir": "models",
          "sources": {
            "sales": {
              "schema": "landing",
              "tables": { "orders": "raw_orders" }
            }
          }
        }"#,
    )
    .expect("write project source declarations");

    let loaded = load_project(project.path()).expect("valid sources should load");
    assert_eq!(loaded.sources["sales"].schema, "landing");
    assert_eq!(loaded.sources["sales"].tables["orders"], "raw_orders");
}

#[test]
fn project_rejects_source_identifiers_that_postgres_would_truncate() {
    let project = TestProject::new();
    let relation_name = "x".repeat(64);
    fs::write(
        project.path().join("dowai_project.json"),
        serde_json::to_vec(&json!({
            "schema_version": 2,
            "name": "fixture",
            "models_dir": "models",
            "sources": {
                "sales": {
                    "schema": "landing",
                    "tables": { "orders": relation_name }
                }
            }
        }))
        .expect("serialize project source declaration"),
    )
    .expect("write project source declaration");

    let error = load_project(project.path()).expect_err("overlong relation names are unsafe");
    assert!(error.message().contains("source table relation"));
}

#[test]
fn project_initialization_rejects_path_components_in_the_name() {
    let workspace = TestProject::new();

    let error = create_project(workspace.path(), "..\\outside").expect_err("reject traversal");

    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
}

#[test]
fn project_initialization_rejects_windows_reserved_names() {
    let workspace = TestProject::new();

    let error = create_project(workspace.path(), "CON.txt").expect_err("reject reserved name");

    assert_eq!(error.code(), "DOWAI_CONFIG_INVALID");
}

#[test]
fn project_initialization_preserves_existing_workspace_configuration() {
    let workspace = TestProject::new();
    let config = workspace.path().join("workspace.json");
    fs::write(
        &config,
        r#"{"schema_version":2,"workspace_name":"custom","profiles":{"source":{}}}"#,
    )
    .expect("write existing workspace config");

    create_project(workspace.path(), "sample").expect("initialize project");

    assert_eq!(
        fs::read_to_string(config).expect("read workspace config"),
        r#"{"schema_version":2,"workspace_name":"custom","profiles":{"source":{}}}"#
    );
}

#[test]
fn dynamic_model_references_fail_instead_of_being_omitted() {
    let project = TestProject::new();
    project.write_model("consumer.sql", "select * from {{ ref(model_name) }}");

    let error = plan(
        project.path(),
        &["consumer.sql".into()],
        ExecutionMode::Automatic,
    )
    .expect_err("dynamic refs cannot be planned safely");

    assert_eq!(error.code(), "DOWAI_PLAN_INVALID");
}

#[test]
fn missing_static_references_fail_validation() {
    let project = TestProject::new();
    project.write_model("consumer.sql", "select * from {{ ref('missing') }}");

    let error = plan(
        project.path(),
        &["consumer.sql".into()],
        ExecutionMode::Automatic,
    )
    .expect_err("missing refs cannot produce a plan");

    assert!(matches!(error, DowaiError::Model(_)));
}

#[test]
fn project_relative_model_paths_work_with_nested_custom_models_directory() {
    let project = TestProject::new();
    fs::write(
        project.path().join("dowai_project.json"),
        r#"{"schema_version":2,"name":"fixture","models_dir":"src/models"}"#,
    )
    .expect("replace config");
    project.write_model("../src/models/base.sql", "select 1 as id");

    let plan = plan(
        project.path(),
        &["src/models/base.sql".into()],
        ExecutionMode::Automatic,
    )
    .expect("project-relative path should resolve from the project root");

    assert_eq!(plan.nodes[0].id, "base.sql");
}
