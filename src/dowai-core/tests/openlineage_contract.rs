//! ENT-005 (D11): o schema OpenLineage é fixado em `specs/openlineage/` por versão e hash.
//! Eventos de exemplo válidos devem passar e eventos inválidos devem ser recusados.

use dowai_core::sha256_hex;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const PINNED_VERSION: &str = "1.55.0";
const PINNED_SCHEMA_SHA256: &str =
    "69f68bee00b9beac88a87059c0102410e7bb05f3f43c46d02a0409831eceb0d2";

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../specs")
}

fn read_json(path: &Path) -> Value {
    let bytes = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_slice(&bytes)
        .unwrap_or_else(|error| panic!("{} is JSON: {error}", path.display()))
}

fn schema_bytes() -> Vec<u8> {
    fs::read(root().join("openlineage/OpenLineage.json")).expect("schema OpenLineage fixado")
}

fn validator() -> jsonschema::Validator {
    let schema = read_json(&root().join("openlineage/OpenLineage.json"));
    jsonschema::validator_for(&schema).expect("schema OpenLineage compila")
}

fn fixtures(group: &str) -> Vec<PathBuf> {
    let directory = root().join("fixtures/openlineage").join(group);
    let mut files: Vec<PathBuf> = fs::read_dir(&directory)
        .unwrap_or_else(|error| panic!("fixtures OpenLineage/{group}: {error}"))
        .map(|entry| entry.expect("entrada de diretório").path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "OpenLineage/{group} precisa de fixtures");
    files
}

#[test]
fn pinned_schema_hash_matches_release() {
    assert_eq!(sha256_hex(&schema_bytes()), PINNED_SCHEMA_SHA256);
    assert_eq!(PINNED_VERSION, "1.55.0");
    let schema = read_json(&root().join("openlineage/OpenLineage.json"));
    assert_eq!(
        schema["$id"].as_str(),
        Some("https://openlineage.io/spec/2-0-2/OpenLineage.json")
    );
}

#[test]
fn valid_run_events_pass_pinned_schema() {
    let schema = validator();
    for file in fixtures("valid") {
        let instance = read_json(&file);
        let errors: Vec<String> = schema
            .iter_errors(&instance)
            .map(|error| error.to_string())
            .collect();
        assert!(
            errors.is_empty(),
            "{} deveria ser válido: {errors:?}",
            file.display()
        );
    }
}

#[test]
fn invalid_run_events_are_rejected() {
    let schema = validator();
    for file in fixtures("invalid") {
        let instance = read_json(&file);
        assert!(
            !schema.is_valid(&instance),
            "{} deveria ser recusado pelo schema",
            file.display()
        );
    }
}
