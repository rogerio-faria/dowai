//! ENT-035: `dowai compatibility check <project_path> [--json]` classifies dbt-style constructs
//! of a DOWAI project as `SUPPORTED`, `PARTIAL` or `UNSUPPORTED`, with a project-relative
//! location and a suggestion (plano_mestre §7.2, AC022/AC023/AC024).
//!
//! The check only reads files. It never renders, executes or imports anything. Symlinks inside
//! the models directory are skipped (they are validated by the planner, ENT-024).

use crate::error::Result;
use crate::project::load_project;
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};

pub const COMPATIBILITY_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CompatibilityStatus {
    Supported,
    Partial,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CompatibilityFinding {
    pub status: CompatibilityStatus,
    pub code: String,
    pub construct: String,
    /// Caminho relativo à raiz do projeto, com separador `/`.
    pub path: String,
    /// Linha 1-indexada; `null` quando o diagnóstico se refere ao arquivo ou diretório inteiro.
    pub line: Option<usize>,
    pub message: String,
    pub suggestion: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CompatibilitySummary {
    pub supported: usize,
    pub partial: usize,
    pub unsupported: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct CompatibilityReport {
    pub schema_version: u32,
    pub project: String,
    /// Pior classificação encontrada: UNSUPPORTED > PARTIAL > SUPPORTED.
    pub status: CompatibilityStatus,
    pub models_checked: usize,
    pub summary: CompatibilitySummary,
    pub findings: Vec<CompatibilityFinding>,
}

/// Resultado da classificação de um único construto, antes da localização.
struct Classification {
    status: CompatibilityStatus,
    code: &'static str,
    construct: String,
    message: String,
    suggestion: &'static str,
}

fn supported(construct: &str) -> Classification {
    Classification {
        status: CompatibilityStatus::Supported,
        code: "DOWAI_COMPAT_SUPPORTED",
        construct: construct.to_owned(),
        message: format!("`{construct}` é suportado."),
        suggestion: "",
    }
}

fn partial(
    code: &'static str,
    construct: &str,
    message: &str,
    suggestion: &'static str,
) -> Classification {
    Classification {
        status: CompatibilityStatus::Partial,
        code,
        construct: construct.to_owned(),
        message: message.to_owned(),
        suggestion,
    }
}

fn unsupported(
    code: &'static str,
    construct: &str,
    message: &str,
    suggestion: &'static str,
) -> Classification {
    Classification {
        status: CompatibilityStatus::Unsupported,
        code,
        construct: construct.to_owned(),
        message: message.to_owned(),
        suggestion,
    }
}

/// Verifica o projeto em `project_root` e devolve o relatório de compatibilidade.
///
/// Erro somente quando o projeto não pode ser carregado (`DOWAI_CONFIG_INVALID`, `DOWAI_PATH_*`
/// e afins); construtos não suportados são diagnósticos do relatório, não falhas.
pub fn check_compatibility(project_root: &Path) -> Result<CompatibilityReport> {
    let project = load_project(project_root)?;
    let mut classified: Vec<(Classification, String, Option<usize>)> = Vec::new();

    for (name, code, construct, message, suggestion) in PROJECT_LEVEL_UNSUPPORTED {
        if project_root.join(name).exists() {
            classified.push((
                unsupported(code, construct, message, suggestion),
                (*name).to_owned(),
                None,
            ));
        }
    }

    let dbt_project = project_root.join("dbt_project.yml");
    if dbt_project.is_file() {
        let text = fs::read_to_string(&dbt_project)?;
        for (index, line) in text.lines().enumerate() {
            let key = line.trim_start();
            if key.starts_with("on-run-start") || key.starts_with("on-run-end") {
                classified.push((
                    unsupported(
                        "DOWAI_COMPAT_HOOK_UNSUPPORTED",
                        "on-run hook",
                        "Hooks de início/fim de execução não são suportados.",
                        "Remova o hook; efeitos externos não são executados por hooks no DOWAI.",
                    ),
                    "dbt_project.yml".to_owned(),
                    Some(index + 1),
                ));
            }
        }
    }

    let models_root = project_root.join(&project.models_dir);
    let mut sql_files = Vec::new();
    if models_root.is_dir() {
        collect_sql_files(&models_root, &mut sql_files)?;
    }
    sql_files.sort();

    for file in &sql_files {
        let relative = relative_path(project_root, file);
        let source = fs::read_to_string(file)?;
        for (classification, line) in scan_model(&source) {
            classified.push((classification, relative.clone(), Some(line)));
        }
    }

    let mut summary = CompatibilitySummary::default();
    let mut findings = Vec::new();
    for (classification, path, line) in classified {
        match classification.status {
            CompatibilityStatus::Supported => summary.supported += 1,
            CompatibilityStatus::Partial => summary.partial += 1,
            CompatibilityStatus::Unsupported => summary.unsupported += 1,
        }
        if classification.status != CompatibilityStatus::Supported {
            findings.push(CompatibilityFinding {
                status: classification.status,
                code: classification.code.to_owned(),
                construct: classification.construct,
                path,
                line,
                message: classification.message,
                suggestion: classification.suggestion.to_owned(),
            });
        }
    }
    findings.sort_by(|a, b| {
        (&a.path, a.line, &a.code, &a.construct).cmp(&(&b.path, b.line, &b.code, &b.construct))
    });

    let status = if summary.unsupported > 0 {
        CompatibilityStatus::Unsupported
    } else if summary.partial > 0 {
        CompatibilityStatus::Partial
    } else {
        CompatibilityStatus::Supported
    };

    Ok(CompatibilityReport {
        schema_version: COMPATIBILITY_SCHEMA_VERSION,
        project: project.name,
        status,
        models_checked: sql_files.len(),
        summary,
        findings,
    })
}

/// Entradas de diretórios e arquivos típicos de projetos dbt que o DOWAI não suporta.
const PROJECT_LEVEL_UNSUPPORTED: &[(&str, &str, &str, &str, &str)] = &[
    (
        "macros",
        "DOWAI_COMPAT_MACROS_UNSUPPORTED",
        "macros/",
        "Diretório de macros não é suportado; macros não são executadas pelo DOWAI.",
        "Substitua as macros por SQL explícito ou por ref()/source().",
    ),
    (
        "snapshots",
        "DOWAI_COMPAT_SNAPSHOTS_UNSUPPORTED",
        "snapshots/",
        "Snapshots não são suportados no escopo inicial.",
        "Remova os snapshots ou modele a historização explicitamente.",
    ),
    (
        "seeds",
        "DOWAI_COMPAT_SEEDS_UNSUPPORTED",
        "seeds/",
        "Seeds não são suportadas no escopo inicial.",
        "Carregue os dados por fonte declarada (source()) ou por etapa explícita.",
    ),
    (
        "packages.yml",
        "DOWAI_COMPAT_PACKAGES_UNSUPPORTED",
        "packages.yml",
        "Packages dbt não são suportados.",
        "Remova a dependência e implemente o SQL necessário diretamente no projeto.",
    ),
];

/// Localiza cada construto de template com início `{{`, `{%`; comentários `{# #}` são ignorados.
fn scan_model(source: &str) -> Vec<(Classification, usize)> {
    let mut found = Vec::new();
    let mut position = 0;
    while let Some(offset) = source[position..].find('{') {
        let start = position + offset;
        let rest = &source[start..];
        let (close, is_expression) = if rest.starts_with("{{") {
            ("}}", true)
        } else if rest.starts_with("{%") {
            ("%}", false)
        } else if rest.starts_with("{#") {
            match source[start + 2..].find("#}") {
                Some(end) => {
                    position = start + 2 + end + 2;
                    continue;
                }
                None => break,
            }
        } else {
            position = start + 1;
            continue;
        };
        let content_start = start + 2;
        let Some(end) = source[content_start..].find(close) else {
            break;
        };
        let content = &source[content_start..content_start + end];
        let line = source[..start].matches('\n').count() + 1;
        let classification = if is_expression {
            Some(classify_expression(content))
        } else {
            classify_directive(content)
        };
        if let Some(classification) = classification {
            found.push((classification, line));
        }
        position = content_start + end + close.len();
    }
    found
}

fn classify_directive(content: &str) -> Option<Classification> {
    let trimmed = content.trim();
    let keyword = trimmed
        .split(|character: char| character.is_whitespace() || character == '(')
        .next()
        .unwrap_or("");
    match keyword {
        "if" => Some(partial(
            "DOWAI_COMPAT_CONDITIONAL_SUBSET",
            "if",
            "Condicional suportada apenas no subconjunto documentado.",
            "Confirme em docs/COMPATIBILITY.md que a condição usa var() com comparação simples ou valor padrão.",
        )),
        "elif" | "else" | "endif" => None,
        "def_var" => Some(supported("def_var (extensão DOWAI)")),
        keyword if keyword.starts_with("end") => None,
        "" => Some(unsupported(
            "DOWAI_COMPAT_BLOCK_UNSUPPORTED",
            "{% %}",
            "Bloco de template vazio não é suportado.",
            "Remova o bloco.",
        )),
        keyword => Some(unsupported(
            "DOWAI_COMPAT_BLOCK_UNSUPPORTED",
            keyword,
            &format!("Bloco de template `{keyword}` não é suportado."),
            "Substitua por SQL explícito ou por ref()/source()/config(materialized=...).",
        )),
    }
}

fn classify_expression(content: &str) -> Classification {
    let trimmed = content.trim();
    if trimmed.starts_with("def_var") {
        return supported("def_var (extensão DOWAI)");
    }
    let name_end = trimmed
        .find(|character: char| {
            !(character.is_ascii_alphanumeric() || character == '_' || character == '.')
        })
        .unwrap_or(trimmed.len());
    let name = &trimmed[..name_end];
    let has_call = trimmed[name_end..].trim_start().starts_with('(');

    if has_call {
        return match name {
            "ref" | "source" => supported(name),
            "config" => classify_config(trimmed),
            "var" => unsupported(
                "DOWAI_COMPAT_EXPRESSION_UNSUPPORTED",
                "var()",
                "var() como expressão de saída não é suportado; só é aceito em condicionais.",
                "Use var() dentro de {% if %}, não como valor impresso no SQL.",
            ),
            _ => unsupported(
                "DOWAI_COMPAT_MACRO_UNSUPPORTED",
                &format!("{name}()"),
                &format!("Macro `{name}` não é suportada."),
                "Substitua a macro por SQL explícito ou por ref()/source(). Macros não são executadas pelo DOWAI.",
            ),
        };
    }

    unsupported(
        "DOWAI_COMPAT_EXPRESSION_UNSUPPORTED",
        &short(trimmed),
        "Expressão de template não suportada.",
        "Substitua a expressão por SQL explícito ou por ref()/source().",
    )
}

/// O DOWAI aceita somente `config(materialized='table'|'view')`; qualquer outra chave é não suportada.
fn classify_config(expression: &str) -> Classification {
    if ["pre_hook", "post_hook", "pre-hook", "post-hook"]
        .iter()
        .any(|hook| expression.contains(hook))
    {
        return unsupported(
            "DOWAI_COMPAT_HOOK_UNSUPPORTED",
            "config(pre/post hook)",
            "Hooks de pre/post execução não são suportados.",
            "Remova o hook; efeitos externos devem ser etapas explícitas do projeto.",
        );
    }
    let arguments = expression
        .find('(')
        .map(|open| expression[open + 1..].trim_end().trim_end_matches(')'))
        .unwrap_or("");
    let keys: Vec<&str> = arguments
        .split(',')
        .filter_map(|argument| {
            let key = argument.split(['=', ':']).next()?.trim();
            (!key.is_empty()).then_some(key)
        })
        .collect();
    if keys.iter().any(|key| *key != "materialized") {
        return unsupported(
            "DOWAI_COMPAT_CONFIG_UNSUPPORTED",
            "config(chave)",
            "Chave de config fora de materialized não é suportada.",
            "Use somente config(materialized='table') ou config(materialized='view').",
        );
    }
    match extract_string_value(arguments, "materialized") {
        Some(value) if value == "table" || value == "view" => {
            supported("config(materialized)")
        }
        Some(value) => unsupported(
            "DOWAI_COMPAT_MATERIALIZATION_UNSUPPORTED",
            &format!("config(materialized='{value}')"),
            &format!("Materialização `{value}` não é suportada; apenas table e view."),
            "Use materialized='table' ou materialized='view'. Incremental depende de contrato futuro.",
        ),
        None => unsupported(
            "DOWAI_COMPAT_CONFIG_UNSUPPORTED",
            "config(materialized)",
            "materialized deve ser um literal string 'table' ou 'view'.",
            "Use config(materialized='table') ou config(materialized='view').",
        ),
    }
}

/// Extrai o literal string de `key = 'valor'` ou `key: "valor"` dentro dos argumentos de `config()`.
fn extract_string_value(arguments: &str, key: &str) -> Option<String> {
    let index = arguments.find(key)?;
    let after = arguments[index + key.len()..].trim_start();
    let after = after
        .strip_prefix('=')
        .or_else(|| after.strip_prefix(':'))?
        .trim_start();
    let quote = after.chars().next().filter(|c| *c == '\'' || *c == '"')?;
    let body = &after[quote.len_utf8()..];
    let end = body.find(quote)?;
    Some(body[..end].to_owned())
}

fn short(text: &str) -> String {
    const LIMIT: usize = 60;
    if text.chars().count() <= LIMIT {
        text.to_owned()
    } else {
        let truncated: String = text.chars().take(LIMIT).collect();
        format!("{truncated}…")
    }
}

fn collect_sql_files(current: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(current)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_symlink() {
            // Symlinks são validados pelo planner (ENT-024); aqui não são seguidos.
            continue;
        }
        if file_type.is_dir() {
            collect_sql_files(&path, files)?;
        } else if file_type.is_file()
            && path.extension().and_then(|value| value.to_str()) == Some("sql")
        {
            files.push(path);
        }
    }
    Ok(())
}

fn relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}
