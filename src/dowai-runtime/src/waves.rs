//! Ondas de execução e detecção de conflitos de variáveis (ENT-120a, ADR 0008 §1 e §4).
//!
//! Uma onda reúne nós cujas dependências (dentro do plano) já estão em ondas anteriores. Nós de
//! uma mesma onda são independentes entre si e poderiam executar em paralelo. Este módulo define
//! apenas a regra de agrupamento e a detecção de conflito; o agendador atual continua sequencial.
//! A consumidora concorrente é ENT-120b (nós SQL) e ENT-120c (nós Python e misto).

use dowai_core::{DowaiError, PlanNode, Result};
use std::collections::{BTreeMap, HashMap};

/// Agrupa os índices de `nodes` em ondas. A onda de um nó é `1 + max(onda das dependências)`
/// dentro do plano; dependências fora do plano são ignoradas. Dentro de cada onda, os índices
/// seguem a ordem do plano. Ciclo é recusado (o planejador já o impede; defesa adicional).
pub(crate) fn waves(nodes: &[PlanNode]) -> Result<Vec<Vec<usize>>> {
    let index_by_id: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(index, node)| (node.id.as_str(), index))
        .collect();
    // 0 = não calculado, 1 = em cálculo (detecção de ciclo), 2 = concluído.
    let mut state = vec![0u8; nodes.len()];
    let mut level = vec![0usize; nodes.len()];
    for start in 0..nodes.len() {
        resolve_level(start, nodes, &index_by_id, &mut state, &mut level)?;
    }
    let deepest = level.iter().copied().max().unwrap_or(0);
    let mut grouped = vec![Vec::new(); deepest + 1];
    for (index, wave) in level.iter().enumerate() {
        grouped[*wave].push(index);
    }
    grouped.retain(|wave| !wave.is_empty());
    Ok(grouped)
}

fn resolve_level(
    index: usize,
    nodes: &[PlanNode],
    index_by_id: &HashMap<&str, usize>,
    state: &mut [u8],
    level: &mut [usize],
) -> Result<usize> {
    match state[index] {
        2 => return Ok(level[index]),
        1 => {
            return Err(DowaiError::Cycle(format!(
                "cycle in execution plan at {}",
                nodes[index].id
            )))
        }
        _ => {}
    }
    state[index] = 1;
    let mut deepest_dependency = None;
    for dependency in &nodes[index].depends_on {
        if let Some(&dependency_index) = index_by_id.get(dependency.as_str()) {
            let dependency_level =
                resolve_level(dependency_index, nodes, index_by_id, state, level)?;
            deepest_dependency = Some(
                deepest_dependency.map_or(dependency_level, |current: usize| {
                    current.max(dependency_level)
                }),
            );
        }
    }
    level[index] = deepest_dependency.map_or(0, |deepest| deepest + 1);
    state[index] = 2;
    Ok(level[index])
}

/// Detecta variáveis gravadas por mais de um nó da mesma onda (ADR 0008 §4). `writes` associa o
/// índice do nó às variáveis que ele grava. Retorna `DOWAI_VARIABLE_CONFLICT` na primeira
/// variável em conflito, em ordem determinística (nome da variável), sem resolução automática.
pub(crate) fn detect_variable_conflicts(
    wave: &[usize],
    writes: &HashMap<usize, Vec<String>>,
    nodes: &[PlanNode],
) -> Result<()> {
    let mut writers: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for &index in wave {
        for variable in writes.get(&index).into_iter().flatten() {
            writers.entry(variable.as_str()).or_default().push(index);
        }
    }
    for (variable, indexes) in writers {
        if indexes.len() > 1 {
            let nodes_text: Vec<&str> = indexes.iter().map(|&i| nodes[i].id.as_str()).collect();
            return Err(DowaiError::VariableConflict(format!(
                "variable {variable} written by nodes of the same wave: {}",
                nodes_text.join(", ")
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, depends_on: &[&str]) -> PlanNode {
        PlanNode {
            id: id.to_owned(),
            path: format!("models/{id}.sql"),
            depends_on: depends_on.iter().map(|d| (*d).to_owned()).collect(),
            sha256: String::new(),
        }
    }

    fn ids(nodes: &[PlanNode], wave: &[usize]) -> Vec<String> {
        wave.iter().map(|&i| nodes[i].id.clone()).collect()
    }

    #[test]
    fn independent_nodes_share_one_wave_in_plan_order() {
        let nodes = vec![
            node("a", &[]),
            node("b", &[]),
            node("c", &[]),
            node("d", &[]),
        ];
        let grouped = waves(&nodes).unwrap();
        assert_eq!(grouped.len(), 1);
        assert_eq!(ids(&nodes, &grouped[0]), ["a", "b", "c", "d"]);
    }

    #[test]
    fn chain_produces_one_node_per_wave() {
        let nodes = vec![node("a", &[]), node("b", &["a"]), node("c", &["b"])];
        let grouped = waves(&nodes).unwrap();
        assert_eq!(grouped.len(), 3);
        assert_eq!(ids(&nodes, &grouped[2]), ["c"]);
    }

    #[test]
    fn diamond_waits_for_the_deepest_dependency() {
        // a -> b -> d ; a -> c ; d depends on b and c: d must be after b.
        let nodes = vec![
            node("a", &[]),
            node("b", &["a"]),
            node("c", &["a"]),
            node("d", &["b", "c"]),
        ];
        let grouped = waves(&nodes).unwrap();
        assert_eq!(grouped.len(), 3);
        assert_eq!(ids(&nodes, &grouped[0]), ["a"]);
        assert_eq!(ids(&nodes, &grouped[1]), ["b", "c"]);
        assert_eq!(ids(&nodes, &grouped[2]), ["d"]);
    }

    #[test]
    fn dependencies_outside_the_plan_are_ignored() {
        let nodes = vec![node("a", &["external"]), node("b", &["a"])];
        let grouped = waves(&nodes).unwrap();
        assert_eq!(grouped.len(), 2);
        assert_eq!(ids(&nodes, &grouped[0]), ["a"]);
    }

    #[test]
    fn cycle_is_refused() {
        let nodes = vec![node("a", &["b"]), node("b", &["a"])];
        let error = waves(&nodes).unwrap_err();
        assert_eq!(error.code(), "DOWAI_PLAN_CYCLE");
    }

    #[test]
    fn empty_plan_has_no_waves() {
        assert!(waves(&[]).unwrap().is_empty());
    }

    #[test]
    fn same_variable_in_one_wave_is_a_conflict() {
        let nodes = vec![node("a", &[]), node("b", &[])];
        let grouped = waves(&nodes).unwrap();
        let writes = HashMap::from([(0, vec!["x".to_owned()]), (1, vec!["x".to_owned()])]);
        let error = detect_variable_conflicts(&grouped[0], &writes, &nodes).unwrap_err();
        assert_eq!(error.code(), "DOWAI_VARIABLE_CONFLICT");
        assert_eq!(error.exit_code(), dowai_core::exit_codes::EXECUTION_FAILED);
    }

    #[test]
    fn distinct_variables_in_one_wave_do_not_conflict() {
        let nodes = vec![node("a", &[]), node("b", &[])];
        let writes = HashMap::from([(0, vec!["x".to_owned()]), (1, vec!["y".to_owned()])]);
        assert!(detect_variable_conflicts(&[0, 1], &writes, &nodes).is_ok());
    }

    #[test]
    fn same_variable_in_different_waves_is_not_a_wave_conflict() {
        let nodes = vec![node("a", &[]), node("b", &["a"])];
        let writes = HashMap::from([(0, vec!["x".to_owned()]), (1, vec!["x".to_owned()])]);
        assert!(detect_variable_conflicts(&[0], &writes, &nodes).is_ok());
        assert!(detect_variable_conflicts(&[1], &writes, &nodes).is_ok());
    }
}
