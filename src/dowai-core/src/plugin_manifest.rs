//! Manifesto de plugin: validação por JSON Schema e assinaturas por análise estática (ENT-052,
//! plano §9.2–9.3).
//!
//! Descoberta nunca importa, compila para execução nem avalia o código do plugin. O manifesto
//! `plugin.json` é validado contra `specs/plugin.schema.json` (embutido em tempo de compilação)
//! e as assinaturas dos entrypoints são extraídas por AST do arquivo `module:function`, dentro
//! do diretório do plugin. Resolução de módulo que escape do diretório (symlink ou traversal)
//! é recusada com `DOWAI_POLICY_PATH`.
//!
//! Permissões não são concedidas aqui: a descoberta apenas registra o que o manifesto declara.

use crate::{sha256_hex, DowaiError, Result};
use rustpython_ast::{self as ast, Mod, Stmt};
use rustpython_parser::{parse, Mode};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Nome do manifesto dentro do diretório do plugin.
pub const MANIFEST_FILE: &str = "plugin.json";

/// Schema público do manifesto (`schema_version` 2), embutido para não depender do checkout.
const PLUGIN_SCHEMA: &str = include_str!("../../../specs/plugin.schema.json");

/// Permissões declaradas no manifesto. Declarar não concede: o runtime nega o não declarado.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PluginPermissions {
    pub network: Vec<String>,
    pub filesystem: Vec<String>,
    pub secrets: Vec<String>,
}

/// Manifesto validado e metadados de origem (plano §9.2: caminho, versão, hash).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    /// Método → `module:function`, em ordem determinística.
    pub entrypoints: BTreeMap<String, String>,
    pub permissions: PluginPermissions,
    pub timeout_seconds: u64,
    pub side_effects: bool,
    pub idempotency_key_required: bool,
    /// SHA-256 dos bytes de `plugin.json`.
    pub manifest_sha256: String,
    /// Caminho absoluto ou relativo ao chamador do `plugin.json` lido.
    pub manifest_path: PathBuf,
}

/// Tipo de parâmetro conforme a assinatura Python.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParameterKind {
    PositionalOnly,
    PositionalOrKeyword,
    VarPositional,
    KeywordOnly,
    VarKeyword,
}

/// Parâmetro extraído por AST. Sem valor padrão ou anotação é apenas registrado como tal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterSignature {
    pub name: String,
    pub kind: ParameterKind,
    pub annotated: bool,
    pub has_default: bool,
}

/// Assinatura de um entrypoint, obtida sem importar o módulo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginSignature {
    pub method: String,
    pub entrypoint: String,
    /// Caminho relativo ao diretório do plugin (determinístico entre máquinas).
    pub source_file: PathBuf,
    pub is_async: bool,
    pub parameters: Vec<ParameterSignature>,
    pub returns_annotated: bool,
}

/// Valida um JSON contra o schema do manifesto. Erros são reunidos em `report.errors`.
pub fn validate_manifest_value(value: &Value) -> Result<()> {
    let schema: Value = serde_json::from_str(PLUGIN_SCHEMA)
        .map_err(|error| DowaiError::Config(format!("schema de plugin inválido: {error}")))?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|error| DowaiError::Config(format!("schema de plugin inválido: {error}")))?;
    let errors: Vec<String> = validator
        .iter_errors(value)
        .map(|error| error.to_string())
        .collect();
    if errors.is_empty() {
        return Ok(());
    }
    Err(DowaiError::Validation {
        message: format!("manifesto de plugin inválido: {}", errors.join("; ")),
        report: json!({ "errors": errors }),
    })
}

/// Lê e valida `<plugin_dir>/plugin.json`. Não importa nem executa nenhum arquivo do plugin.
pub fn read_manifest(plugin_dir: &Path) -> Result<PluginManifest> {
    let manifest_path = plugin_dir.join(MANIFEST_FILE);
    let metadata = fs::symlink_metadata(&manifest_path)?;
    if !metadata.file_type().is_file() {
        return Err(DowaiError::Path(format!(
            "{} deve ser arquivo regular (symlink ou diretório recusado)",
            manifest_path.display()
        )));
    }
    let bytes = fs::read(&manifest_path)?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|error| DowaiError::Validation {
        message: format!("manifesto de plugin não é JSON válido: {error}"),
        report: json!({ "errors": [error.to_string()] }),
    })?;
    validate_manifest_value(&value)?;

    let entrypoints = value["entrypoints"]
        .as_object()
        .map(|object| {
            object
                .iter()
                .map(|(method, target)| {
                    (
                        method.clone(),
                        target.as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let permissions = PluginPermissions {
        network: string_list(&value["permissions"]["network"]),
        filesystem: string_list(&value["permissions"]["filesystem"]),
        secrets: string_list(&value["permissions"]["secrets"]),
    };

    Ok(PluginManifest {
        id: string_field(&value["id"]),
        version: string_field(&value["version"]),
        entrypoints,
        permissions,
        timeout_seconds: value["timeout_seconds"].as_u64().unwrap_or_default(),
        side_effects: value["side_effects"].as_bool().unwrap_or_default(),
        idempotency_key_required: value["idempotency_key_required"]
            .as_bool()
            .unwrap_or_default(),
        manifest_sha256: sha256_hex(&bytes),
        manifest_path,
    })
}

/// Extrai a assinatura de cada entrypoint do manifesto por análise estática do arquivo.
///
/// O módulo `a.b:fn` é resolvido para `a/b.py` ou `a/b/__init__.py` dentro de `plugin_dir`.
/// Caminhos que saiam do diretório (inclusive por symlink) são recusados. A função precisa
/// ser uma definição de nível de módulo; a última definição com o mesmo nome prevalece, como
/// no Python.
pub fn extract_signatures(
    plugin_dir: &Path,
    manifest: &PluginManifest,
) -> Result<Vec<PluginSignature>> {
    let mut signatures = Vec::with_capacity(manifest.entrypoints.len());
    for (method, entrypoint) in &manifest.entrypoints {
        signatures.push(signature_for(plugin_dir, method, entrypoint)?);
    }
    Ok(signatures)
}

fn signature_for(plugin_dir: &Path, method: &str, entrypoint: &str) -> Result<PluginSignature> {
    let Some((module, function)) = entrypoint.split_once(':') else {
        return Err(DowaiError::Validation {
            message: format!("entrypoint {method} sem separador ':'"),
            report: json!({ "method": method, "entrypoint": entrypoint }),
        });
    };
    let source_file = resolve_module(plugin_dir, method, module)?;
    let absolute = plugin_dir.join(&source_file);
    let source = fs::read_to_string(&absolute)?;

    let parsed =
        parse(&source, Mode::Module, &source_file.display().to_string()).map_err(|error| {
            DowaiError::Validation {
                message: format!(
                    "{} não pôde ser analisado sem execução: {}",
                    source_file.display(),
                    error.error
                ),
                report: json!({ "method": method, "file": source_file.display().to_string() }),
            }
        })?;
    let Mod::Module(module_ast) = parsed else {
        return Err(DowaiError::Validation {
            message: format!("módulo inesperado em {}", source_file.display()),
            report: json!({ "method": method }),
        });
    };

    let mut found: Option<PluginSignature> = None;
    for statement in &module_ast.body {
        let signature = match statement {
            Stmt::FunctionDef(definition) if definition.name.as_str() == function => Some(build(
                method,
                entrypoint,
                &source_file,
                false,
                &definition.args,
                definition.returns.is_some(),
            )),
            Stmt::AsyncFunctionDef(definition) if definition.name.as_str() == function => {
                Some(build(
                    method,
                    entrypoint,
                    &source_file,
                    true,
                    &definition.args,
                    definition.returns.is_some(),
                ))
            }
            _ => None,
        };
        if signature.is_some() {
            found = signature;
        }
    }
    found.ok_or_else(|| DowaiError::Validation {
        message: format!(
            "função '{function}' do entrypoint {method} não é definida em nível de módulo em {}",
            source_file.display()
        ),
        report: json!({ "method": method, "entrypoint": entrypoint }),
    })
}

fn build(
    method: &str,
    entrypoint: &str,
    source_file: &Path,
    is_async: bool,
    arguments: &ast::Arguments,
    returns_annotated: bool,
) -> PluginSignature {
    let mut parameters = Vec::new();
    for arg in &arguments.posonlyargs {
        parameters.push(with_default(ParameterKind::PositionalOnly, arg));
    }
    for arg in &arguments.args {
        parameters.push(with_default(ParameterKind::PositionalOrKeyword, arg));
    }
    if let Some(vararg) = &arguments.vararg {
        parameters.push(bare(ParameterKind::VarPositional, vararg));
    }
    for arg in &arguments.kwonlyargs {
        parameters.push(with_default(ParameterKind::KeywordOnly, arg));
    }
    if let Some(kwarg) = &arguments.kwarg {
        parameters.push(bare(ParameterKind::VarKeyword, kwarg));
    }
    PluginSignature {
        method: method.to_string(),
        entrypoint: entrypoint.to_string(),
        source_file: source_file.to_path_buf(),
        is_async,
        parameters,
        returns_annotated,
    }
}

fn with_default(kind: ParameterKind, arg: &ast::ArgWithDefault) -> ParameterSignature {
    ParameterSignature {
        name: arg.def.arg.as_str().to_string(),
        kind,
        annotated: arg.def.annotation.is_some(),
        has_default: arg.default.is_some(),
    }
}

fn bare(kind: ParameterKind, arg: &ast::Arg) -> ParameterSignature {
    ParameterSignature {
        name: arg.arg.as_str().to_string(),
        kind,
        annotated: arg.annotation.is_some(),
        has_default: false,
    }
}

/// Resolve `a.b` para um arquivo relativo dentro de `plugin_dir`, sem sair do diretório.
fn resolve_module(plugin_dir: &Path, method: &str, module: &str) -> Result<PathBuf> {
    let parts: Vec<&str> = module.split('.').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return Err(DowaiError::Path(format!(
            "módulo do entrypoint {method} inválido: '{module}'"
        )));
    }
    let base: PathBuf = parts.iter().collect();
    let candidates = [base.with_extension("py"), base.join("__init__.py")];
    let canonical_root = fs::canonicalize(plugin_dir)?;
    for candidate in candidates {
        let absolute = plugin_dir.join(&candidate);
        if fs::symlink_metadata(&absolute).is_err() {
            continue;
        }
        let resolved = fs::canonicalize(&absolute)?;
        if !resolved.starts_with(&canonical_root) {
            return Err(DowaiError::Path(format!(
                "módulo do entrypoint {method} escapa o diretório do plugin (symlink ou traversal)"
            )));
        }
        if resolved.is_file() {
            return Ok(candidate);
        }
    }
    Err(DowaiError::Validation {
        message: format!("módulo '{module}' do entrypoint {method} não encontrado no plugin"),
        report: json!({ "method": method, "module": module }),
    })
}

fn string_field(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}
