//! ENT-023 (plano §14.2): demonstração de `plan` automático e manual com snapshots JSON
//! versionados em `tests/contracts/plan-demo/` e hashes SHA-256 em `SHA256SUMS`.
//!
//! O planejamento é estático: nenhum modelo Python é executado e o projeto de demonstração
//! não recebe escrita (nenhum arquivo novo, sessão ou banco).

use dowai_core::{plan, sha256_hex, ExecutionMode};
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn demo_project() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plan-demo")
}

fn snapshots_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/contracts/plan-demo")
}

fn read_snapshot(name: &str) -> Vec<u8> {
    fs::read(snapshots_dir().join(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
}

fn directory_entries(path: &Path) -> BTreeSet<String> {
    fs::read_dir(path)
        .expect("read demo project")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

fn targets(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

#[test]
fn snapshot_hashes_match_manifest() {
    let manifest = String::from_utf8(read_snapshot("SHA256SUMS")).expect("manifest is UTF-8");
    let mut checked = 0;
    for line in manifest.lines() {
        let (hash, name) = line.split_once("  ").expect("manifest line format");
        assert_eq!(
            sha256_hex(&read_snapshot(name)),
            hash,
            "hash divergente para {name}"
        );
        checked += 1;
    }
    assert_eq!(checked, 2, "manifesto deve cobrir os dois snapshots");
}

#[test]
fn automatic_plan_matches_snapshot_without_running_python() {
    let before = directory_entries(&demo_project());

    let result = plan(
        &demo_project(),
        &targets(&["models/comissao.py"]),
        ExecutionMode::Automatic,
    )
    .expect("automatic plan");
    let actual = serde_json::to_value(&result).expect("serialize plan");
    let expected: Value =
        serde_json::from_slice(&read_snapshot("automatic-comissao.json")).expect("snapshot JSON");

    assert_eq!(actual, expected);
    assert_eq!(
        directory_entries(&demo_project()),
        before,
        "plan não pode criar artefatos no projeto"
    );
}

#[test]
fn manual_plan_preserves_given_order_and_reports_ignored_dependency() {
    let result = plan(
        &demo_project(),
        &targets(&["models/arq1.sql", "models/arq10.sql", "models/arq7.py"]),
        ExecutionMode::Manual,
    )
    .expect("manual plan");
    let actual = serde_json::to_value(&result).expect("serialize plan");
    let expected: Value = serde_json::from_slice(&read_snapshot("manual-arq1-arq10-arq7.json"))
        .expect("snapshot JSON");

    assert_eq!(actual, expected);

    let ids: Vec<&str> = result.nodes.iter().map(|node| node.id.as_str()).collect();
    assert_eq!(
        ids,
        ["arq1.sql", "arq10.sql", "arq7.py"],
        "ordem dada deve ser preservada"
    );
    assert_eq!(result.warnings.len(), 1);
    assert!(result.warnings[0].contains("arq1.sql depends on arq7.py"));
}
