//! Ondas de execução e detecção de conflitos de variáveis (ENT-120a, ADR 0008 §1 e §4).
//!
//! Uma onda reúne nós cujas dependências (dentro do plano) já estão em ondas anteriores. Nós de
//! uma mesma onda são independentes entre si. Este módulo define a regra de agrupamento, a
//! detecção de conflito e o limite de concorrência de uma onda (ENT-120b, ADR 0008 §2). O
//! caminho concorrente cobre somente nós SQL no modo automático; nós Python e o modo manual
//! continuam sequenciais (ENT-120c).

use dowai_core::{DowaiError, PlanNode, Result};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;

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

/// Runs `count` items with at most `limit` of them in flight (ADR 0008 §2). Items are claimed in
/// index order by `min(limit, count)` scoped threads. `work` returns the item's result and whether
/// the wave must halt. After a halt, or once `cancelled()` holds, no unclaimed item starts. The
/// result has one slot per item, in index order; `None` marks an item that never started.
///
/// A panic in `work` is propagated to the caller after every thread has finished.
pub(crate) fn run_concurrently<T, W, C>(
    count: usize,
    limit: usize,
    cancelled: C,
    work: W,
) -> Vec<Option<T>>
where
    T: Send,
    W: Fn(usize) -> (T, bool) + Sync,
    C: Fn() -> bool + Sync,
{
    let next = AtomicUsize::new(0);
    let halted = AtomicBool::new(false);
    let workers = limit.min(count).max(1);
    let joined = thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        if halted.load(Ordering::SeqCst) || cancelled() {
                            break;
                        }
                        let index = next.fetch_add(1, Ordering::SeqCst);
                        if index >= count {
                            break;
                        }
                        let (value, halt) = work(index);
                        if halt {
                            halted.store(true, Ordering::SeqCst);
                        }
                        done.push((index, value));
                    }
                    done
                })
            })
            .collect();
        handles
            .into_iter()
            .map(thread::ScopedJoinHandle::join)
            .collect::<Vec<_>>()
    });
    let mut slots: Vec<Option<T>> = (0..count).map(|_| None).collect();
    for outcome in joined {
        match outcome {
            Ok(done) => {
                for (index, value) in done {
                    slots[index] = Some(value);
                }
            }
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
    slots
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Waits until `in_flight` reaches `target` (or a generous deadline), so overlap is observed
    /// without relying on sleeps of fixed length.
    fn wait_for_overlap(in_flight: &AtomicUsize, target: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while in_flight.load(Ordering::SeqCst) < target && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// Runs `count` items and returns the maximum number seen in flight at once.
    fn peak_in_flight(count: usize, limit: usize, target: usize) -> (usize, Vec<Option<usize>>) {
        let in_flight = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let slots = run_concurrently(
            count,
            limit,
            || false,
            |index| {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                wait_for_overlap(&in_flight, target);
                in_flight.fetch_sub(1, Ordering::SeqCst);
                (index, false)
            },
        );
        (peak.load(Ordering::SeqCst), slots)
    }

    #[test]
    fn items_overlap_up_to_the_limit() {
        let (peak, slots) = peak_in_flight(4, 4, 4);
        assert_eq!(peak, 4);
        assert_eq!(slots, [Some(0), Some(1), Some(2), Some(3)]);
    }

    #[test]
    fn concurrency_never_exceeds_the_limit() {
        // The target is the limit: every worker waits for a full set of peers and none exceeds it.
        let (peak, slots) = peak_in_flight(4, 2, 2);
        assert_eq!(peak, 2);
        assert!(slots.iter().all(Option::is_some));
    }

    #[test]
    fn limit_one_runs_items_in_index_order() {
        let order = std::sync::Mutex::new(Vec::new());
        let slots = run_concurrently(
            3,
            1,
            || false,
            |index| {
                order.lock().expect("order lock").push(index);
                (index, false)
            },
        );
        assert_eq!(*order.lock().expect("order lock"), [0, 1, 2]);
        assert_eq!(slots, [Some(0), Some(1), Some(2)]);
    }

    #[test]
    fn halt_prevents_unclaimed_items_from_starting() {
        // With one worker, items are claimed strictly in order: item 1 halts, so 2 and 3 never run.
        let slots = run_concurrently(4, 1, || false, |index| (index, index == 1));
        assert_eq!(slots, [Some(0), Some(1), None, None]);
    }

    #[test]
    fn cancellation_prevents_every_item_from_starting() {
        let slots = run_concurrently(3, 2, || true, |index| (index, false));
        assert_eq!(slots, [None, None, None]);
    }

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
