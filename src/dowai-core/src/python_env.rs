//! Ambiente Python do projeto: detecção de divergência entre `dowai_project.json` e
//! `pyproject.toml` (plano §8.4, D04, ENT-046).
//!
//! `dowai_project.json` (`python.dependencies`) é a fonte canônica. Este módulo apenas lê o
//! `pyproject.toml` opcional, compara os nomes canônicos (PEP 503) e devolve o diagnóstico. Não
//! mescla listas, não reescreve arquivos e não instala pacotes. A criação do venv e o lockfile
//! são feitos por `scripts/python-env.sh` com `uv`, fora do run produtivo.

use crate::package_policy::normalize_package_name;
use crate::Result;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

/// Nome do arquivo de interoperabilidade Python na raiz do projeto.
pub const PYPROJECT_FILE: &str = "pyproject.toml";

/// Diferença entre as dependências declaradas no projeto e no `pyproject.toml`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DependencyDivergence {
    /// Nomes canônicos presentes só em `dowai_project.json`.
    pub only_in_project: Vec<String>,
    /// Nomes canônicos presentes só em `pyproject.toml`.
    pub only_in_pyproject: Vec<String>,
}

impl DependencyDivergence {
    /// Indica se as duas fontes declaram o mesmo conjunto de pacotes.
    pub fn is_empty(&self) -> bool {
        self.only_in_project.is_empty() && self.only_in_pyproject.is_empty()
    }

    /// Resumo legível para diagnóstico, sem trechos de segredo (apenas nomes de pacotes).
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.only_in_project.is_empty() {
            parts.push(format!(
                "somente em dowai_project.json: {}",
                self.only_in_project.join(", ")
            ));
        }
        if !self.only_in_pyproject.is_empty() {
            parts.push(format!(
                "somente em pyproject.toml: {}",
                self.only_in_pyproject.join(", ")
            ));
        }
        parts.join("; ")
    }
}

/// Lê `[project].dependencies` de um `pyproject.toml`.
///
/// Devolve `Ok(None)` quando o arquivo não existe ou não declara a tabela `[project]`. Não há
/// parser TOML completo no workspace: a leitura aceita arrays de strings simples (inclusive em
/// várias linhas) dentro de `[project]`. Formas não reconhecidas resultam em `None` e geram
/// apenas o diagnóstico de leitura pelo chamador, nunca merge silencioso.
pub fn read_pyproject_dependencies(project_root: &Path) -> Result<Option<Vec<String>>> {
    let path = project_root.join(PYPROJECT_FILE);
    if !path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path)?;
    Ok(parse_project_dependencies(&text))
}

fn parse_project_dependencies(text: &str) -> Option<Vec<String>> {
    let mut in_project = false;
    let mut in_array = false;
    let mut found_table = false;
    let mut collected = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if !in_array {
            if line.starts_with('[') {
                in_project = line == "[project]";
                found_table |= in_project;
                continue;
            }
            if !in_project {
                continue;
            }
            let Some(rest) = line.strip_prefix("dependencies") else {
                continue;
            };
            let Some(rest) = rest.trim_start().strip_prefix('=') else {
                continue;
            };
            let rest = rest.trim_start();
            if !rest.starts_with('[') {
                return None;
            }
            in_array = true;
            collect_strings(rest, &mut collected);
            if rest.contains(']') {
                return Some(collected);
            }
        } else {
            collect_strings(line, &mut collected);
            if line.contains(']') {
                return Some(collected);
            }
        }
    }
    if found_table && in_array {
        return None;
    }
    if found_table {
        Some(Vec::new())
    } else {
        None
    }
}

fn collect_strings(fragment: &str, out: &mut Vec<String>) {
    let mut chars = fragment.chars();
    while let Some(character) = chars.next() {
        if character == '"' || character == '\'' {
            let mut value = String::new();
            for inner in chars.by_ref() {
                if inner == character {
                    break;
                }
                value.push(inner);
            }
            if !value.trim().is_empty() {
                out.push(value);
            }
        }
    }
}

/// Compara dois conjuntos de declarações pelos nomes canônicos, ignorando versões e marcadores.
pub fn compare_dependencies(
    declared_in_project: &[String],
    declared_in_pyproject: &[String],
) -> Result<DependencyDivergence> {
    let project_names = canonical_names(declared_in_project)?;
    let pyproject_names = canonical_names(declared_in_pyproject)?;
    Ok(DependencyDivergence {
        only_in_project: project_names
            .difference(&pyproject_names)
            .cloned()
            .collect(),
        only_in_pyproject: pyproject_names
            .difference(&project_names)
            .cloned()
            .collect(),
    })
}

fn canonical_names(declarations: &[String]) -> Result<BTreeSet<String>> {
    declarations
        .iter()
        .map(|declaration| normalize_package_name(declaration))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_single_line_and_multiline_arrays() {
        let text = "[build-system]\nrequires = [\"setuptools\"]\n\n[project]\nname = \"x\"\ndependencies = [\"Pandas>=2\", 'sqlalchemy[asyncio]']\n";
        assert_eq!(
            parse_project_dependencies(text),
            Some(strings(&["Pandas>=2", "sqlalchemy[asyncio]"]))
        );

        let multi =
            "[project]\ndependencies = [\n  \"pyarrow\",  # comentário\n  \"polars==1.2\",\n]\n";
        assert_eq!(
            parse_project_dependencies(multi),
            Some(strings(&["pyarrow", "polars==1.2"]))
        );
    }

    #[test]
    fn project_without_dependencies_is_empty_and_missing_table_is_none() {
        assert_eq!(
            parse_project_dependencies("[project]\nname = \"x\"\n"),
            Some(Vec::new())
        );
        assert_eq!(parse_project_dependencies("[tool.x]\na = 1\n"), None);
    }

    #[test]
    fn unrecognized_dependencies_form_is_not_guessed() {
        assert_eq!(
            parse_project_dependencies("[project]\ndependencies = dynamic\n"),
            None
        );
    }

    #[test]
    fn equal_sets_with_different_spelling_do_not_diverge() {
        let divergence = compare_dependencies(
            &strings(&["My_Pkg>=1", "pandas"]),
            &strings(&["my-pkg==1.0", "Pandas[sql]"]),
        )
        .unwrap();
        assert!(divergence.is_empty(), "{divergence:?}");
    }

    #[test]
    fn divergence_lists_each_side_and_describes_it() {
        let divergence = compare_dependencies(
            &strings(&["pandas", "polars"]),
            &strings(&["pandas", "pyarrow"]),
        )
        .unwrap();
        assert_eq!(divergence.only_in_project, strings(&["polars"]));
        assert_eq!(divergence.only_in_pyproject, strings(&["pyarrow"]));
        assert_eq!(
            divergence.describe(),
            "somente em dowai_project.json: polars; somente em pyproject.toml: pyarrow"
        );
    }
}
