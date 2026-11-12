//! Dicionário semântico JSON global e local (ENT-069, plano §11.2; AC020, AC021).
//!
//! O dicionário global fica na raiz do workspace (`data_dictionary.json`) e o local em
//! `data_docs/dictionary.json` do projeto. Ambos seguem `specs/data_dictionary.schema.json`
//! (embutido em tempo de compilação). A mesclagem é feita por ID completo da métrica: a entrada
//! local substitui integralmente a global, e cada métrica mesclada registra a origem e o arquivo
//! de onde veio.
//!
//! Além do schema, referências são verificadas: `grain` precisa listar identificadores não
//! vazios, `source_tables` precisa ser `tabela` ou `esquema.tabela` e um sinônimo não pode
//! apontar para duas métricas diferentes nem coincidir com o ID de outra métrica. Fórmulas são
//! apenas texto para revisão; nada aqui as executa nem consulta o banco.

use crate::{DowaiError, Result};
use regex::Regex;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Nome do dicionário global, na raiz do workspace.
pub const GLOBAL_DICTIONARY_FILE: &str = "data_dictionary.json";
/// Caminho do dicionário local, relativo à raiz do projeto.
pub const LOCAL_DICTIONARY_PATH: &str = "data_docs/dictionary.json";

const WORKSPACE_FILE: &str = "workspace.json";
const PROJECT_FILE: &str = "dowai_project.json";

/// Schema público do dicionário (`schema_version` 2), embutido para não depender do checkout.
const DICTIONARY_SCHEMA: &str = include_str!("../../../specs/data_dictionary.schema.json");

/// Origem de uma métrica após a mesclagem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DictionaryOrigin {
    Global,
    Local,
}

impl DictionaryOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Local => "local",
        }
    }
}

/// Métrica validada, com os campos do schema versão 2.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Metric {
    pub description: String,
    pub synonyms: Vec<String>,
    pub formula: Option<String>,
    pub source_tables: Vec<String>,
    pub grain: Vec<String>,
    pub filters: Vec<String>,
    pub notes: Option<String>,
}

/// Dicionário de um único arquivo (global ou local), já validado.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dictionary {
    pub path: PathBuf,
    pub metrics: BTreeMap<String, Metric>,
}

/// Métrica no resultado da mesclagem, com origem auditável.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergedMetric {
    pub origin: DictionaryOrigin,
    pub source: PathBuf,
    /// `true` quando a métrica local substituiu uma entrada global de mesmo ID.
    pub replaces_global: bool,
    pub metric: Metric,
}

/// Resultado da mesclagem: uma entrada por ID completo.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MergedDictionary {
    pub metrics: BTreeMap<String, MergedMetric>,
}

/// Localização dos dicionários a partir de um diretório de trabalho.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DictionaryPaths {
    pub global: Option<PathBuf>,
    pub local: Option<PathBuf>,
}

/// Localiza o workspace (ancestral com `workspace.json`) e o projeto (ancestral com
/// `dowai_project.json`). Os caminhos são devolvidos mesmo quando o arquivo não existe;
/// use [`load_dictionary`] para ler e validar.
pub fn dictionary_paths_from(start: &Path) -> DictionaryPaths {
    let global = start
        .ancestors()
        .find(|candidate| candidate.join(WORKSPACE_FILE).is_file())
        .map(|root| root.join(GLOBAL_DICTIONARY_FILE));
    let local = start
        .ancestors()
        .find(|candidate| candidate.join(PROJECT_FILE).is_file())
        .map(|root| root.join(LOCAL_DICTIONARY_PATH));
    DictionaryPaths {
        global: global.filter(|path| path.is_file()),
        local: local.filter(|path| path.is_file()),
    }
}

/// Lê, valida contra o schema e verifica referências de um dicionário em disco.
/// Arquivo ausente é erro de E/S; conteúdo inválido é recusado com `DOWAI_CONFIG_INVALID`.
pub fn load_dictionary(path: &Path) -> Result<Dictionary> {
    let bytes = fs::read(path)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| DowaiError::Validation {
        message: format!("dicionário {} não é JSON válido: {error}", path.display()),
        report: json!({ "path": path.display().to_string(), "errors": [error.to_string()] }),
    })?;
    parse_dictionary(path, &value)
}

/// Valida um dicionário já carregado em memória. `path` é usado apenas para origem e mensagens.
pub fn parse_dictionary(path: &Path, value: &Value) -> Result<Dictionary> {
    validate_schema(path, value)?;
    let raw = value
        .get("metrics")
        .and_then(Value::as_object)
        .ok_or_else(|| invalid(path, vec!["campo 'metrics' ausente".to_owned()]))?;

    let mut metrics = BTreeMap::new();
    let mut errors = Vec::new();
    for (id, entry) in raw {
        match parse_metric(id, entry) {
            Ok(metric) => {
                metrics.insert(id.clone(), metric);
            }
            Err(mut found) => errors.append(&mut found),
        }
    }
    check_synonyms(&metrics, &mut errors);
    if errors.is_empty() {
        Ok(Dictionary {
            path: path.to_path_buf(),
            metrics,
        })
    } else {
        Err(invalid(path, errors))
    }
}

/// Mescla o dicionário global e o local por ID completo. O local substitui a entrada global
/// de mesmo ID (sem mesclar campos) e a origem fica registrada em cada métrica.
///
/// Recusa com `DOWAI_CONFIG_INVALID` quando, no resultado, um sinônimo aparece em mais de uma
/// métrica ou coincide com o ID de outra métrica: a referência ficaria ambígua.
pub fn merge(global: Option<&Dictionary>, local: Option<&Dictionary>) -> Result<MergedDictionary> {
    let mut merged = MergedDictionary::default();
    if let Some(global) = global {
        for (id, metric) in &global.metrics {
            merged.metrics.insert(
                id.clone(),
                MergedMetric {
                    origin: DictionaryOrigin::Global,
                    source: global.path.clone(),
                    replaces_global: false,
                    metric: metric.clone(),
                },
            );
        }
    }
    if let Some(local) = local {
        for (id, metric) in &local.metrics {
            let replaces_global = merged.metrics.contains_key(id);
            merged.metrics.insert(
                id.clone(),
                MergedMetric {
                    origin: DictionaryOrigin::Local,
                    source: local.path.clone(),
                    replaces_global,
                    metric: metric.clone(),
                },
            );
        }
    }
    let mut errors = Vec::new();
    let view: BTreeMap<String, Metric> = merged
        .metrics
        .iter()
        .map(|(id, entry)| (id.clone(), entry.metric.clone()))
        .collect();
    check_synonyms(&view, &mut errors);
    if errors.is_empty() {
        Ok(merged)
    } else {
        Err(DowaiError::Validation {
            message: format!(
                "dicionários mesclados com referências ambíguas: {}",
                errors.join("; ")
            ),
            report: json!({ "errors": errors }),
        })
    }
}

fn validate_schema(path: &Path, value: &Value) -> Result<()> {
    let schema: Value = serde_json::from_str(DICTIONARY_SCHEMA)
        .map_err(|error| DowaiError::Config(format!("schema de dicionário inválido: {error}")))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| DowaiError::Config(format!("schema de dicionário inválido: {error}")))?;
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(invalid(path, errors))
    }
}

fn parse_metric(id: &str, entry: &Value) -> std::result::Result<Metric, Vec<String>> {
    let mut errors = Vec::new();
    let description = text_field(entry, "description").unwrap_or_default();
    let synonyms = string_list(entry, "synonyms");
    let formula = entry
        .get("formula")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let source_tables = string_list(entry, "source_tables");
    let grain = string_list(entry, "grain");
    let filters = string_list(entry, "filters");
    let notes = entry
        .get("notes")
        .and_then(Value::as_str)
        .map(str::to_owned);

    if entry.get("grain").is_some() && grain.is_empty() {
        errors.push(format!(
            "métrica '{id}': grain vazio; informe ao menos uma coluna"
        ));
    }
    for column in &grain {
        if !identifier_re().is_match(column) {
            errors.push(format!(
                "métrica '{id}': coluna de grain '{column}' não é um identificador válido"
            ));
        }
    }
    for table in &source_tables {
        if !table_re().is_match(table) {
            errors.push(format!(
                "métrica '{id}': referência de tabela '{table}' inválida (use 'tabela' ou 'esquema.tabela')"
            ));
        }
    }

    if errors.is_empty() {
        Ok(Metric {
            description,
            synonyms,
            formula,
            source_tables,
            grain,
            filters,
            notes,
        })
    } else {
        Err(errors)
    }
}

fn check_synonyms(metrics: &BTreeMap<String, Metric>, errors: &mut Vec<String>) {
    let ids: BTreeSet<&str> = metrics.keys().map(String::as_str).collect();
    let mut owners: BTreeMap<&str, &str> = BTreeMap::new();
    for (id, metric) in metrics {
        for synonym in &metric.synonyms {
            if ids.contains(synonym.as_str()) {
                errors.push(format!(
                    "sinônimo '{synonym}' da métrica '{id}' coincide com o ID de outra métrica"
                ));
                continue;
            }
            match owners.get(synonym.as_str()) {
                Some(owner) if *owner != id.as_str() => errors.push(format!(
                    "sinônimo '{synonym}' aparece nas métricas '{owner}' e '{id}'"
                )),
                Some(_) => {}
                None => {
                    owners.insert(synonym.as_str(), id.as_str());
                }
            }
        }
    }
}

fn identifier_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*$").expect("static regex"))
}

fn table_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)?$").expect("static regex")
    })
}

fn text_field(entry: &Value, key: &str) -> Option<String> {
    entry.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn string_list(entry: &Value, key: &str) -> Vec<String> {
    entry
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn invalid(path: &Path, errors: Vec<String>) -> DowaiError {
    DowaiError::Validation {
        message: format!(
            "dicionário {} inválido: {}",
            path.display(),
            errors.join("; ")
        ),
        report: json!({ "path": path.display().to_string(), "errors": errors }),
    }
}
