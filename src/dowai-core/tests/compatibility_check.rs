//! ENT-035: `check_compatibility` classifies dbt-style constructs as SUPPORTED, PARTIAL or
//! UNSUPPORTED with project-relative locations, without executing anything.

use dowai_core::{check_compatibility, create_project, CompatibilityStatus};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

struct Sandbox(PathBuf);

impl Sandbox {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("dowai-compat-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&root).expect("create sandbox");
        Self(root)
    }

    fn project(&self) -> PathBuf {
        create_project(&self.0, "demo").expect("create project")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create parent");
    }
    fs::write(path, content).expect("write file");
}

#[test]
fn supported_constructs_produce_supported_report() {
    let sandbox = Sandbox::new();
    let root = sandbox.project();
    write(
        &root,
        "models/orders.sql",
        "{{ config(materialized='table') }}\nselect * from {{ ref('base') }}\n\
         union all select * from {{ source('raw', 'orders') }}\n",
    );

    let report = check_compatibility(&root).expect("check");

    assert_eq!(report.status, CompatibilityStatus::Supported);
    assert_eq!(report.models_checked, 1);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    assert_eq!(report.summary.unsupported, 0);
    assert_eq!(report.summary.partial, 0);
    assert!(report.summary.supported >= 3);
}

#[test]
fn unsupported_macro_is_located_with_line_and_suggestion() {
    let sandbox = Sandbox::new();
    let root = sandbox.project();
    write(
        &root,
        "models/star.sql",
        "select 1 as a\nfrom {{ ref('base') }}\nwhere {{ dbt_utils.not_null() }}\n",
    );

    let report = check_compatibility(&root).expect("check");

    assert_eq!(report.status, CompatibilityStatus::Unsupported);
    assert_eq!(report.findings.len(), 1);
    let finding = &report.findings[0];
    assert_eq!(finding.status, CompatibilityStatus::Unsupported);
    assert_eq!(finding.code, "DOWAI_COMPAT_MACRO_UNSUPPORTED");
    assert_eq!(finding.construct, "dbt_utils.not_null()");
    assert_eq!(finding.path, "models/star.sql");
    assert_eq!(finding.line, Some(3));
    assert!(!finding.suggestion.is_empty());
}

#[test]
fn incremental_and_hooks_are_unsupported() {
    let sandbox = Sandbox::new();
    let root = sandbox.project();
    write(
        &root,
        "models/inc.sql",
        "{{ config(materialized='incremental') }}\nselect 1\n",
    );
    write(
        &root,
        "models/hook.sql",
        "{{ config(materialized='table', post_hook='analyze') }}\nselect 1\n",
    );

    let report = check_compatibility(&root).expect("check");

    assert_eq!(report.status, CompatibilityStatus::Unsupported);
    let codes: Vec<&str> = report.findings.iter().map(|f| f.code.as_str()).collect();
    assert!(
        codes.contains(&"DOWAI_COMPAT_MATERIALIZATION_UNSUPPORTED"),
        "{codes:?}"
    );
    assert!(
        codes.contains(&"DOWAI_COMPAT_HOOK_UNSUPPORTED"),
        "{codes:?}"
    );
    assert!(report
        .findings
        .iter()
        .any(|f| f.path == "models/hook.sql" && f.line == Some(1)));
}

#[test]
fn conditional_blocks_are_partial() {
    let sandbox = Sandbox::new();
    let root = sandbox.project();
    write(
        &root,
        "models/cond.sql",
        "{% if var('flag') == 'x' %}\nselect 1\n{% else %}\nselect 2\n{% endif %}\n",
    );

    let report = check_compatibility(&root).expect("check");

    assert_eq!(report.status, CompatibilityStatus::Partial);
    assert_eq!(report.summary.partial, 1);
    assert_eq!(report.summary.unsupported, 0);
    assert_eq!(report.findings[0].code, "DOWAI_COMPAT_CONDITIONAL_SUBSET");
    assert_eq!(report.findings[0].status, CompatibilityStatus::Partial);
}

#[test]
fn macros_inside_comments_are_ignored() {
    let sandbox = Sandbox::new();
    let root = sandbox.project();
    write(
        &root,
        "models/commented.sql",
        "{# {{ dbt_utils.star() }} #}\nselect 1\n",
    );

    let report = check_compatibility(&root).expect("check");

    assert_eq!(report.status, CompatibilityStatus::Supported);
    assert!(report.findings.is_empty(), "{:?}", report.findings);
}

#[test]
fn dbt_directories_and_packages_are_reported_at_file_level() {
    let sandbox = Sandbox::new();
    let root = sandbox.project();
    write(
        &root,
        "macros/helpers.sql",
        "{% macro m() %}1{% endmacro %}\n",
    );
    write(&root, "packages.yml", "packages: []\n");
    write(
        &root,
        "dbt_project.yml",
        "name: x\non-run-start:\n  - 'select 1'\n",
    );

    let report = check_compatibility(&root).expect("check");

    assert_eq!(report.status, CompatibilityStatus::Unsupported);
    let file_level: Vec<(&str, Option<usize>)> = report
        .findings
        .iter()
        .map(|f| (f.path.as_str(), f.line))
        .collect();
    assert!(file_level.contains(&("macros", None)), "{file_level:?}");
    assert!(
        file_level.contains(&("packages.yml", None)),
        "{file_level:?}"
    );
    assert!(
        file_level.contains(&("dbt_project.yml", Some(2))),
        "{file_level:?}"
    );
}

#[test]
fn report_is_deterministic_across_runs() {
    let sandbox = Sandbox::new();
    let root = sandbox.project();
    write(&root, "models/b.sql", "{{ dbt_utils.a() }}\n");
    write(&root, "models/a.sql", "{{ dbt_utils.b() }}\n");

    let first = serde_json::to_string(&check_compatibility(&root).expect("first")).expect("json");
    let second = serde_json::to_string(&check_compatibility(&root).expect("second")).expect("json");

    assert_eq!(first, second);
    let report = check_compatibility(&root).expect("check");
    let paths: Vec<&str> = report.findings.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, vec!["models/a.sql", "models/b.sql"]);
}

#[test]
fn project_that_cannot_be_loaded_is_an_error() {
    let sandbox = Sandbox::new();
    let empty = sandbox.0.join("not-a-project");
    fs::create_dir_all(&empty).expect("create dir");

    assert!(check_compatibility(&empty).is_err());
}
