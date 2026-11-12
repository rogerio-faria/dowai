//! Contrato do dicionário semântico global/local (ENT-069; AC020 e AC021).
//!
//! Cobre: fixtures válidas e inválidas do schema, mesclagem por ID completo com local
//! substituindo global e origem registrada, e recusa de grain, referência de tabela e
//! sinônimo ambíguo.

use dowai_core::{
    dictionary_paths_from, load_dictionary, merge_dictionaries, parse_dictionary, DictionaryOrigin,
    DowaiError,
};
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

fn repo_file(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("dowai-dict-{name}-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).expect("scratch directory");
    root
}

fn metric(description: &str) -> Value {
    json!({ "description": description })
}

fn dictionary(metrics: Value) -> Value {
    json!({ "schema_version": 2, "metrics": metrics })
}

fn write(path: &Path, value: &Value) -> PathBuf {
    fs::create_dir_all(path.parent().expect("parent")).expect("parent dir");
    fs::write(path, serde_json::to_vec_pretty(value).expect("json")).expect("write dictionary");
    path.to_path_buf()
}

fn refused_code(error: &DowaiError) -> &'static str {
    error.code()
}

#[test]
fn valid_fixtures_load_and_invalid_fixtures_are_refused() {
    let base = repo_file("specs/fixtures/data_dictionary");
    for entry in fs::read_dir(base.join("valid")).expect("valid fixtures") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|ext| ext == "json") {
            load_dictionary(&path)
                .unwrap_or_else(|error| panic!("{} deve ser aceito: {error}", path.display()));
        }
    }
    for entry in fs::read_dir(base.join("invalid")).expect("invalid fixtures") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|ext| ext == "json") {
            let error =
                load_dictionary(&path).expect_err(&format!("{} deve ser recusado", path.display()));
            assert_eq!(refused_code(&error), "DOWAI_CONFIG_INVALID");
        }
    }
}

#[test]
fn local_entry_replaces_global_by_full_id_and_keeps_origin() {
    let root = scratch("merge");
    let global_path = write(
        &root.join("global/data_dictionary.json"),
        &dictionary(json!({
            "faturamento": { "description": "global", "synonyms": ["receita"] },
            "clientes": metric("clientes globais")
        })),
    );
    let local_path = write(
        &root.join("projeto/data_docs/dictionary.json"),
        &dictionary(json!({
            "faturamento": { "description": "local", "grain": ["id_venda"] }
        })),
    );
    let global = load_dictionary(&global_path).expect("global");
    let local = load_dictionary(&local_path).expect("local");

    let merged = merge_dictionaries(Some(&global), Some(&local)).expect("merge");

    let faturamento = &merged.metrics["faturamento"];
    assert_eq!(faturamento.origin, DictionaryOrigin::Local);
    assert_eq!(faturamento.source, local_path);
    assert!(faturamento.replaces_global);
    assert_eq!(faturamento.metric.description, "local");
    // Substituição integral: campos só presentes na global não são herdados.
    assert!(faturamento.metric.synonyms.is_empty());
    assert_eq!(faturamento.metric.grain, vec!["id_venda".to_owned()]);

    let clientes = &merged.metrics["clientes"];
    assert_eq!(clientes.origin, DictionaryOrigin::Global);
    assert_eq!(clientes.source, global_path);
    assert!(!clientes.replaces_global);

    fs::remove_dir_all(root).ok();
}

#[test]
fn merge_without_local_keeps_global_entries() {
    let global = parse_dictionary(
        Path::new("global.json"),
        &dictionary(json!({ "clientes": metric("clientes") })),
    )
    .expect("global");
    let merged = merge_dictionaries(Some(&global), None).expect("merge");
    assert_eq!(merged.metrics.len(), 1);
    assert_eq!(merged.metrics["clientes"].origin, DictionaryOrigin::Global);
}

#[test]
fn invalid_grain_is_refused() {
    for grain in [
        json!([]),
        json!(["id venda"]),
        json!(["tab.id"]),
        json!([""]),
    ] {
        let value = dictionary(json!({
            "vendas": { "description": "vendas", "grain": grain }
        }));
        let error = parse_dictionary(Path::new("d.json"), &value)
            .expect_err("grain inválido deve ser recusado");
        assert_eq!(
            refused_code(&error),
            "DOWAI_CONFIG_INVALID",
            "grain {grain}"
        );
    }
}

#[test]
fn invalid_source_table_reference_is_refused() {
    for table in ["tab-vendas", "a.b.c", "1tabela", "esquema."] {
        let value = dictionary(json!({
            "vendas": { "description": "vendas", "source_tables": [table] }
        }));
        let error = parse_dictionary(Path::new("d.json"), &value)
            .expect_err("referência inválida deve ser recusada");
        assert_eq!(
            refused_code(&error),
            "DOWAI_CONFIG_INVALID",
            "tabela {table}"
        );
    }
}

#[test]
fn valid_schema_qualified_source_table_is_accepted() {
    let value = dictionary(json!({
        "vendas": { "description": "vendas", "source_tables": ["fiscal.tab_vendas", "tab_x"] }
    }));
    parse_dictionary(Path::new("d.json"), &value).expect("referência válida");
}

#[test]
fn ambiguous_synonym_after_merge_is_refused() {
    let global = parse_dictionary(
        Path::new("global.json"),
        &dictionary(json!({ "faturamento": { "description": "f", "synonyms": ["receita"] } })),
    )
    .expect("global");
    let local = parse_dictionary(
        Path::new("local.json"),
        &dictionary(json!({ "receita": { "description": "outra" } })),
    )
    .expect("local");
    let error = merge_dictionaries(Some(&global), Some(&local))
        .expect_err("sinônimo que coincide com ID deve ser recusado");
    assert_eq!(refused_code(&error), "DOWAI_CONFIG_INVALID");
}

#[test]
fn synonym_shared_by_two_metrics_is_refused() {
    let value = dictionary(json!({
        "a": { "description": "a", "synonyms": ["x"] },
        "b": { "description": "b", "synonyms": ["x"] }
    }));
    let error = parse_dictionary(Path::new("d.json"), &value).expect_err("ambíguo");
    assert_eq!(refused_code(&error), "DOWAI_CONFIG_INVALID");
}

#[test]
fn paths_are_discovered_from_workspace_and_project() {
    let root = scratch("paths");
    let workspace = root.join("ws");
    let project = workspace.join("projects/fat");
    fs::create_dir_all(&project).expect("project");
    fs::write(workspace.join("workspace.json"), "{}").expect("workspace");
    fs::write(project.join("dowai_project.json"), "{}").expect("project file");

    let none = dictionary_paths_from(&project);
    assert!(none.global.is_none() && none.local.is_none());

    write(
        &workspace.join("data_dictionary.json"),
        &dictionary(json!({})),
    );
    write(
        &project.join("data_docs/dictionary.json"),
        &dictionary(json!({})),
    );
    let found = dictionary_paths_from(&project);
    assert_eq!(found.global, Some(workspace.join("data_dictionary.json")));
    assert_eq!(found.local, Some(project.join("data_docs/dictionary.json")));

    fs::remove_dir_all(root).ok();
}
