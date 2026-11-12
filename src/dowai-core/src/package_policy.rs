//! Política de pacotes Python por projeto (plano §8.4, D17, ENT-045).
//!
//! Decide quais pacotes um projeto pode usar a partir de três entradas: a política escolhida
//! (`python.package_policy`), as declarações de `python.dependencies` e, quando existir, a lista
//! administrativa. O projeto nunca amplia a lista administrativa: o conjunto efetivo é a
//! interseção entre declarado e administrativo.
//!
//! Este módulo não instala nem importa pacotes; a instalação e a verificação de venv/lockfile
//! pertencem a ENT-046.

use crate::{DowaiError, Result};
use std::collections::BTreeSet;

/// Valores aceitos em `python.package_policy`.
pub const PACKAGE_POLICIES: [&str; 3] = [
    "declared_and_authorized",
    "declared_only",
    "admin_allowlist",
];

/// Política de pacotes Python.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackagePolicy {
    /// Default: exige declaração no projeto e autorização administrativa explícita.
    DeclaredAndAuthorized,
    /// Declaração no projeto basta; a lista administrativa, se existir, intersecta.
    DeclaredOnly,
    /// Somente pacotes da lista administrativa que também estejam declarados.
    AdminAllowlist,
}

impl PackagePolicy {
    /// Converte o valor do manifesto para a política. Valor desconhecido é erro de configuração.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "declared_and_authorized" => Ok(Self::DeclaredAndAuthorized),
            "declared_only" => Ok(Self::DeclaredOnly),
            "admin_allowlist" => Ok(Self::AdminAllowlist),
            _ => Err(DowaiError::Config(
                "python.package_policy is not supported".to_owned(),
            )),
        }
    }
}

/// Extrai e normaliza o nome canônico de uma declaração (PEP 508 simplificada, PEP 503 na
/// normalização). Extras, versões, marcadores e URLs são descartados.
///
/// `"Pandas>=2"` e `"pandas[sql] ; python_version>'3'"` viram `pandas`; `"My_Pkg.Name"` vira
/// `my-pkg-name`.
pub fn normalize_package_name(requirement: &str) -> Result<String> {
    let trimmed = requirement.trim();
    let end = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or(trimmed.len());
    let name = &trimmed[..end];
    let well_formed = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .last()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if !well_formed {
        return Err(DowaiError::Config(format!(
            "invalid Python package declaration: {requirement}"
        )));
    }

    let mut normalized = String::with_capacity(name.len());
    let mut in_separator = false;
    for c in name.chars() {
        if matches!(c, '-' | '_' | '.') {
            if !in_separator {
                normalized.push('-');
            }
            in_separator = true;
        } else {
            normalized.push(c.to_ascii_lowercase());
            in_separator = false;
        }
    }
    Ok(normalized)
}

fn normalize_all(requirements: &[String]) -> Result<BTreeSet<String>> {
    requirements
        .iter()
        .map(|requirement| normalize_package_name(requirement))
        .collect()
}

/// Conjunto de pacotes efetivamente permitidos pela política.
///
/// * Sem lista administrativa: `declared_only` usa as declarações; as demais políticas não
///   autorizam nenhum pacote.
/// * Com lista administrativa: o resultado é `declarados ∩ administrativos`, em todas as políticas.
pub fn effective_packages(
    policy: PackagePolicy,
    declared: &[String],
    administrative_allowlist: Option<&[String]>,
) -> Result<BTreeSet<String>> {
    let declared = normalize_all(declared)?;
    match (policy, administrative_allowlist) {
        (PackagePolicy::DeclaredOnly, None) => Ok(declared),
        (_, None) => Ok(BTreeSet::new()),
        (_, Some(allowlist)) => {
            let administrative = normalize_all(allowlist)?;
            Ok(declared.intersection(&administrative).cloned().collect())
        }
    }
}

/// Verifica se um pacote solicitado pode ser usado. Recusa com `DOWAI_POLICY_DENIED`, distinguindo
/// pacote não declarado de pacote declarado mas não autorizado.
pub fn authorize_package(
    policy: PackagePolicy,
    declared: &[String],
    administrative_allowlist: Option<&[String]>,
    requested: &str,
) -> Result<()> {
    let name = normalize_package_name(requested)?;
    if !normalize_all(declared)?.contains(&name) {
        return Err(DowaiError::Policy(format!(
            "package `{name}` is not declared in python.dependencies"
        )));
    }
    if effective_packages(policy, declared, administrative_allowlist)?.contains(&name) {
        Ok(())
    } else {
        Err(DowaiError::Policy(format!(
            "package `{name}` is not authorized by the administrative package policy"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn normalizes_names_by_pep_503_and_ignores_specifiers() {
        assert_eq!(normalize_package_name("Pandas>=2").unwrap(), "pandas");
        assert_eq!(normalize_package_name("polars[fast]>=1").unwrap(), "polars");
        assert_eq!(
            normalize_package_name("pandas[sql] ; python_version>'3'").unwrap(),
            "pandas"
        );
        assert_eq!(
            normalize_package_name("My_Pkg.Name").unwrap(),
            "my-pkg-name"
        );
        assert_eq!(normalize_package_name("pkg @ https://x").unwrap(), "pkg");
    }

    #[test]
    fn rejects_malformed_declarations() {
        assert!(normalize_package_name("").is_err());
        assert!(normalize_package_name("-x").is_err());
        assert!(normalize_package_name(">=2").is_err());
    }

    #[test]
    fn parses_only_known_policies() {
        assert_eq!(
            PackagePolicy::parse("declared_only").unwrap(),
            PackagePolicy::DeclaredOnly
        );
        assert!(PackagePolicy::parse("anything_goes").is_err());
    }

    #[test]
    fn undeclared_package_is_denied_with_stable_code() {
        let error = authorize_package(
            PackagePolicy::DeclaredOnly,
            &list(&["pandas"]),
            None,
            "requests",
        )
        .expect_err("undeclared package must be refused");

        assert_eq!(error.code(), "DOWAI_POLICY_DENIED");
        assert!(error.to_string().contains("not declared"));
    }

    #[test]
    fn declared_only_without_administrative_list_allows_declared_package() {
        authorize_package(
            PackagePolicy::DeclaredOnly,
            &list(&["Pandas>=2"]),
            None,
            "pandas",
        )
        .expect("declared package is allowed");
    }

    #[test]
    fn default_policy_without_administrative_list_authorizes_nothing() {
        let error = authorize_package(
            PackagePolicy::DeclaredAndAuthorized,
            &list(&["pandas"]),
            None,
            "pandas",
        )
        .expect_err("authorization requires an administrative allowlist");

        assert_eq!(error.code(), "DOWAI_POLICY_DENIED");
    }

    #[test]
    fn project_cannot_expand_administrative_allowlist() {
        let administrative = list(&["pandas"]);
        let error = authorize_package(
            PackagePolicy::DeclaredAndAuthorized,
            &list(&["pandas", "requests"]),
            Some(&administrative),
            "requests",
        )
        .expect_err("project declaration outside the admin allowlist must be refused");

        assert_eq!(error.code(), "DOWAI_POLICY_DENIED");
        assert!(error.to_string().contains("not authorized"));

        let effective = effective_packages(
            PackagePolicy::DeclaredOnly,
            &list(&["pandas", "requests"]),
            Some(&administrative),
        )
        .unwrap();
        assert_eq!(effective, BTreeSet::from(["pandas".to_owned()]));
    }

    #[test]
    fn administrative_allowlist_entry_not_declared_by_project_is_refused() {
        let administrative = list(&["pandas", "polars"]);
        let error = authorize_package(
            PackagePolicy::AdminAllowlist,
            &list(&["pandas"]),
            Some(&administrative),
            "polars",
        )
        .expect_err("a package must be declared by the project to be used");

        assert!(error.to_string().contains("not declared"));
    }

    #[test]
    fn authorized_package_is_allowed_under_admin_allowlist() {
        let administrative = list(&["pyarrow"]);
        authorize_package(
            PackagePolicy::AdminAllowlist,
            &list(&["PyArrow>=16"]),
            Some(&administrative),
            "pyarrow",
        )
        .expect("declared and administratively authorized package is allowed");
    }
}
