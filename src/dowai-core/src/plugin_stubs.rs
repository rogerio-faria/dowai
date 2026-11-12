//! ENT-055: `dowai plugins sync` gera stubs `.pyi` tipados para os plugins efetivos (plano §9.4).
//!
//! - Entrada: somente descoberta por manifesto e assinaturas por AST (`extract_signatures`).
//!   Nenhum módulo de plugin é importado, compilado ou executado.
//! - Overrides: a mesma regra do dispatcher (`plugins.allow_local_override`). Plugin local com
//!   o ID do global substitui o global inteiro.
//! - Saída: `<projeto>/.dowai/stubs/dowai_plugins/<id>.pyi` e `__init__.pyi`. Todo o conteúdo é
//!   montado em memória antes de qualquer escrita; cada arquivo vai para um temporário e depois
//!   é movido com `rename`. `.pyi` que não corresponde a um plugin efetivo é removido.
//! - Determinismo: ordem por ID e método, sem data, sem caminho absoluto no conteúdo.
//! - Parâmetros sem anotação, ou com anotação que não pode ser reproduzida com segurança, usam
//!   `Any` e geram aviso.

use crate::plugin_manifest::{
    extract_signatures, ParameterKind, ParameterSignature, PluginSignature,
};
use crate::plugin_registry::{DiscoveredPlugin, PluginCatalog, PluginOrigin};
use crate::{DowaiError, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub const STUBS_SCHEMA_VERSION: u32 = 1;
/// Diretório de estado local do projeto (plano §3, ignorado pelo Git).
pub const DOWAI_LOCAL_DIR: &str = ".dowai";
/// Pacote de stubs dentro de `.dowai/stubs/`. Os stubs são importados como `dowai_plugins.<id>`.
pub const STUB_PACKAGE: &str = "dowai_plugins";
const INIT_FILE: &str = "__init__.pyi";

const PYTHON_KEYWORDS: &[&str] = &[
    "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from", "global", "if", "import",
    "in", "is", "lambda", "nonlocal", "not", "or", "pass", "raise", "return", "try", "while",
    "with", "yield",
];

/// Nomes que podem aparecer em anotações reproduzidas. Qualquer outro nome vira `Any`.
const REPRODUCIBLE_NAMES: &[&str] = &[
    "Any",
    "Optional",
    "Union",
    "None",
    "str",
    "int",
    "float",
    "bool",
    "bytes",
    "bytearray",
    "list",
    "dict",
    "tuple",
    "set",
    "frozenset",
];

/// Aviso de geração. Não interrompe o sync; aparece no relatório.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StubWarning {
    pub code: &'static str,
    pub plugin: String,
    pub method: Option<String>,
    pub message: String,
}

/// Conteúdo gerado em memória, antes de qualquer escrita.
#[derive(Debug)]
pub struct RenderedStubs {
    /// Nome do arquivo (`email.pyi`, `__init__.pyi`) → conteúdo, em ordem determinística.
    pub files: BTreeMap<String, String>,
    pub plugins: Vec<String>,
    pub warnings: Vec<StubWarning>,
}

/// Resultado de `dowai plugins sync`.
#[derive(Debug)]
pub struct SyncReport {
    pub stubs_dir: PathBuf,
    pub plugins: Vec<String>,
    pub written: Vec<String>,
    pub unchanged: Vec<String>,
    pub removed: Vec<String>,
    pub warnings: Vec<StubWarning>,
}

impl SyncReport {
    pub fn to_json(&self) -> Value {
        json!({
            "schema_version": STUBS_SCHEMA_VERSION,
            "stubs_dir": self.stubs_dir.display().to_string(),
            "plugins": self.plugins,
            "written": self.written,
            "unchanged": self.unchanged,
            "removed": self.removed,
            "warnings": self.warnings.iter().map(warning_json).collect::<Vec<_>>(),
        })
    }
}

fn warning_json(warning: &StubWarning) -> Value {
    json!({
        "code": warning.code,
        "plugin": warning.plugin,
        "method": warning.method,
        "message": warning.message,
    })
}

/// Plugin efetivo por ID: um por ID, com a mesma regra de `resolve_call`.
fn effective_plugins(
    catalog: &PluginCatalog,
    allow_local_override: bool,
) -> Result<Vec<&DiscoveredPlugin>> {
    let mut by_id: BTreeMap<&str, Vec<&DiscoveredPlugin>> = BTreeMap::new();
    for plugin in &catalog.plugins {
        by_id
            .entry(plugin.manifest.id.as_str())
            .or_default()
            .push(plugin);
    }
    let mut effective = Vec::with_capacity(by_id.len());
    for (id, candidates) in by_id {
        match candidates.as_slice() {
            [only] => effective.push(*only),
            [_, _] => {
                let local = candidates
                    .iter()
                    .copied()
                    .find(|plugin| plugin.origin == PluginOrigin::Project);
                let global = candidates
                    .iter()
                    .copied()
                    .find(|plugin| plugin.origin == PluginOrigin::Global);
                match (local, global) {
                    (Some(local), Some(_)) if allow_local_override => effective.push(local),
                    (Some(_), Some(_)) => {
                        return Err(DowaiError::Policy(format!(
                            "override local do plugin {id} recusado: plugins.allow_local_override = false"
                        )))
                    }
                    _ => {
                        return Err(DowaiError::Config(format!(
                            "plugin {id} duplicado no mesmo escopo"
                        )))
                    }
                }
            }
            _ => {
                return Err(DowaiError::Config(format!(
                    "plugin {id} duplicado no mesmo escopo"
                )))
            }
        }
    }
    Ok(effective)
}

fn is_python_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let starts_ok = chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic());
    starts_ok
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
        && !PYTHON_KEYWORDS.contains(&name)
}

/// Aceita anotação somente com nomes de `REPRODUCIBLE_NAMES`, subscritos, `|` e vírgulas.
/// Nomes de módulo, strings (forward references) e expressões livres não são reproduzidos.
fn reproducible_annotation(annotation: &str) -> bool {
    let only_allowed_chars = annotation
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '[' | ']' | ',' | ' ' | '|'));
    !annotation.trim().is_empty()
        && only_allowed_chars
        && annotation
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|token| !token.is_empty())
            .all(|token| REPRODUCIBLE_NAMES.contains(&token))
}

fn render_parameter(
    plugin: &str,
    method: &str,
    parameter: &ParameterSignature,
    warnings: &mut Vec<StubWarning>,
) -> String {
    let annotation = match &parameter.annotation {
        Some(text) if reproducible_annotation(text) => text.clone(),
        Some(_) => {
            warnings.push(StubWarning {
                code: "DOWAI_STUB_TYPE_NOT_REPRODUCED",
                plugin: plugin.to_owned(),
                method: Some(method.to_owned()),
                message: format!(
                    "anotação do parâmetro {} não reproduzida no stub; usa Any",
                    parameter.name
                ),
            });
            "Any".to_owned()
        }
        None => {
            warnings.push(StubWarning {
                code: "DOWAI_STUB_UNTYPED_PARAM",
                plugin: plugin.to_owned(),
                method: Some(method.to_owned()),
                message: format!("parâmetro {} sem anotação; usa Any", parameter.name),
            });
            "Any".to_owned()
        }
    };
    let default = if parameter.has_default { " = ..." } else { "" };
    format!("{}: {annotation}{default}", parameter.name)
}

/// Uma função do stub. Parâmetros posicionais-only e `*args` não são chamáveis por palavra-chave
/// (o worker chama `entrypoint(**payload)`), então são omitidos com aviso.
fn render_function(
    plugin: &str,
    signature: &PluginSignature,
    warnings: &mut Vec<StubWarning>,
) -> String {
    let mut named = Vec::new();
    let mut var_keyword = None;
    for parameter in &signature.parameters {
        match parameter.kind {
            ParameterKind::PositionalOnly | ParameterKind::VarPositional => {
                warnings.push(StubWarning {
                    code: "DOWAI_STUB_PARAM_NOT_KEYWORD",
                    plugin: plugin.to_owned(),
                    method: Some(signature.method.clone()),
                    message: format!(
                        "parâmetro {} não é chamável por palavra-chave; omitido do stub",
                        parameter.name
                    ),
                });
            }
            ParameterKind::PositionalOrKeyword | ParameterKind::KeywordOnly => {
                named.push(render_parameter(
                    plugin,
                    &signature.method,
                    parameter,
                    warnings,
                ));
            }
            ParameterKind::VarKeyword => {
                var_keyword = Some(format!("**{}: Any", parameter.name));
            }
        }
    }

    let mut arguments = Vec::new();
    if !named.is_empty() {
        arguments.push("*".to_owned());
        arguments.extend(named);
    }
    arguments.extend(var_keyword);
    let keyword = if signature.is_async {
        "async def"
    } else {
        "def"
    };
    format!(
        "{keyword} {}({}) -> Any: ...",
        signature.method,
        arguments.join(", ")
    )
}

fn render_plugin(plugin: &DiscoveredPlugin, warnings: &mut Vec<StubWarning>) -> Result<String> {
    let manifest = &plugin.manifest;
    let plugin_dir = manifest
        .manifest_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let signatures = extract_signatures(plugin_dir, manifest)?;

    let mut body = Vec::new();
    for signature in &signatures {
        if !is_python_identifier(&signature.method) {
            warnings.push(StubWarning {
                code: "DOWAI_STUB_METHOD_SKIPPED",
                plugin: manifest.id.clone(),
                method: Some(signature.method.clone()),
                message: "método não é identificador Python; omitido do stub".to_owned(),
            });
            continue;
        }
        body.push(render_function(&manifest.id, signature, warnings));
    }

    let mut text = format!(
        "# Gerado por `dowai plugins sync` (ENT-055). Não editar manualmente.\n\
         # plugin: {} {} origem={} manifest_sha256={}\n\
         from typing import Any, Optional, Union\n",
        manifest.id,
        manifest.version,
        plugin.origin.as_str(),
        manifest.manifest_sha256
    );
    for function in body {
        text.push('\n');
        text.push_str(&function);
        text.push('\n');
    }
    Ok(text)
}

/// Monta todo o pacote de stubs em memória. Não grava nada e não importa plugins.
pub fn render_stubs(catalog: &PluginCatalog, allow_local_override: bool) -> Result<RenderedStubs> {
    let effective = effective_plugins(catalog, allow_local_override)?;
    let mut files = BTreeMap::new();
    let mut plugins = Vec::new();
    let mut warnings = Vec::new();
    let mut init =
        String::from("# Gerado por `dowai plugins sync` (ENT-055). Não editar manualmente.\n");

    for plugin in effective {
        let id = plugin.manifest.id.as_str();
        if !is_python_identifier(id) {
            warnings.push(StubWarning {
                code: "DOWAI_STUB_ID_SKIPPED",
                plugin: id.to_owned(),
                method: None,
                message: "ID não é identificador Python; plugin omitido do stub".to_owned(),
            });
            continue;
        }
        files.insert(format!("{id}.pyi"), render_plugin(plugin, &mut warnings)?);
        init.push_str(&format!("from . import {id} as {id}\n"));
        plugins.push(id.to_owned());
    }
    files.insert(INIT_FILE.to_owned(), init);
    Ok(RenderedStubs {
        files,
        plugins,
        warnings,
    })
}

/// Recusa diretório de stub que seja symlink (plano §9.3; `DOWAI_POLICY_PATH`).
fn ensure_real_dir(path: &Path) -> Result<()> {
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(DowaiError::Path(format!(
                "diretório de stubs recusado por symlink: {}",
                path.display()
            )));
        }
    }
    fs::create_dir_all(path)?;
    Ok(())
}

/// Grava `content` em `dir/name` por arquivo temporário seguido de `rename`.
fn write_atomic(dir: &Path, name: &str, content: &str) -> Result<()> {
    let temporary = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    // Remove resto de execução anterior; `create_new` nunca segue um symlink existente.
    let _ = fs::remove_file(&temporary);
    {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    if let Err(error) = fs::rename(&temporary, dir.join(name)) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

/// Sincroniza `<project_dir>/.dowai/stubs/dowai_plugins/`. Idempotente: arquivo com o mesmo
/// conteúdo não é regravado; `.pyi` de plugin fora do conjunto efetivo é removido.
pub fn sync_stubs(
    project_dir: &Path,
    catalog: &PluginCatalog,
    allow_local_override: bool,
) -> Result<SyncReport> {
    let rendered = render_stubs(catalog, allow_local_override)?;

    let local = project_dir.join(DOWAI_LOCAL_DIR);
    let stubs = local.join("stubs");
    let package = stubs.join(STUB_PACKAGE);
    for dir in [&local, &stubs, &package] {
        ensure_real_dir(dir)?;
    }

    let mut written = Vec::new();
    let mut unchanged = Vec::new();
    for (name, content) in &rendered.files {
        let target = package.join(name);
        if fs::read(&target).ok().as_deref() == Some(content.as_bytes()) {
            unchanged.push(name.clone());
            continue;
        }
        write_atomic(&package, name, content)?;
        written.push(name.clone());
    }

    let expected: BTreeSet<&str> = rendered.files.keys().map(String::as_str).collect();
    let mut removed = Vec::new();
    for entry in fs::read_dir(&package)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // Temporários de execuções interrompidas.
        if name.starts_with('.') && name.contains(".tmp-") {
            let _ = fs::remove_file(entry.path());
            continue;
        }
        if name.ends_with(".pyi") && !expected.contains(name.as_str()) {
            fs::remove_file(entry.path())?;
            removed.push(name);
        }
    }
    removed.sort();

    Ok(SyncReport {
        stubs_dir: package,
        plugins: rendered.plugins,
        written,
        unchanged,
        removed,
        warnings: rendered.warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reproduces_only_builtin_and_typing_annotations() {
        assert!(reproducible_annotation("str"));
        assert!(reproducible_annotation("list[str]"));
        assert!(reproducible_annotation("dict[str, Any]"));
        assert!(reproducible_annotation("str | None"));
        assert!(reproducible_annotation("Optional[int]"));
        assert!(!reproducible_annotation("Path"));
        assert!(!reproducible_annotation("os.PathLike"));
        assert!(!reproducible_annotation("'Forward'"));
        assert!(!reproducible_annotation("Literal[3]"));
        assert!(!reproducible_annotation("str\nint"));
        assert!(!reproducible_annotation(""));
    }

    #[test]
    fn identifiers_reject_keywords_and_invalid_names() {
        assert!(is_python_identifier("email"));
        assert!(is_python_identifier("_x1"));
        assert!(!is_python_identifier("class"));
        assert!(!is_python_identifier("1abc"));
        assert!(!is_python_identifier("send-mail"));
        assert!(!is_python_identifier(""));
    }
}
