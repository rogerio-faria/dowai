//! Limites de paralelismo e recursos (ENT-038, ADR 0005 / D06).
//!
//! Regras vigentes:
//! - `runtime.max_parallel_nodes` e `runtime.max_worker_processes`: o scheduler atual executa
//!   os nós em sequência e um único worker Python por vez. O limite é respeitado (concorrência
//!   efetiva 1). Quando o plano tem nós independentes e o limite é maior que 1, a diferença é
//!   registrada em `warnings`, porque paralelismo entre nós ainda não está ativo.
//! - `runtime.max_memory_mb`: aplicado a cada worker Python como `RLIMIT_AS` no Linux (memória
//!   virtual, estado `partial`). Fora do Linux, `not_enforced` até o Job Object (ENT-093).
//! - `runtime.max_cpu_percent` abaixo de 100: sem enforcement de percentual (`not_enforced`).
//! - `runtime.max_threads`, `python.max_threads` e `runtime.max_inflight_bytes` (ENT-104): não
//!   há enforcement (threads não são limitadas e o buffer de datasets Python é por linhas). Cada
//!   um gera aviso `not_enforced` quando se aplica ao run (`max_threads` ao run com nós;
//!   `python.max_threads` e `max_inflight_bytes` ao run com nós Python).
//! - `python.resource_enforcement = require`: um limite solicitado com estado `not_enforced`
//!   impede o início do worker, com `DOWAI_RESOURCE_UNSUPPORTED`, antes de qualquer código do
//!   usuário. Com `best_effort` (padrão), o estado é apenas reportado em `warnings`.

use dowai_core::{DowaiError, PlanNode, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Estado de aplicação de um limite, conforme D06.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Enforcement {
    Partial,
    NotEnforced,
}

impl Enforcement {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Partial => "partial",
            Self::NotEnforced => "not_enforced",
        }
    }
}

/// Estado do limite de memória de cada worker Python nesta plataforma.
pub(crate) fn memory_enforcement() -> Enforcement {
    if cfg!(target_os = "linux") {
        Enforcement::Partial
    } else {
        Enforcement::NotEnforced
    }
}

/// Estado do percentual de CPU. Não há enforcement nesta versão (D06, Job Object pendente).
pub(crate) fn cpu_enforcement() -> Enforcement {
    Enforcement::NotEnforced
}

/// Limites aplicados a um worker Python. `None` significa que o limite não se aplica.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WorkerLimits {
    pub memory_bytes: Option<u64>,
}

fn effective_u64(configuration: &Value, group: &str, name: &str) -> Result<u64> {
    configuration["effective"][group][name]
        .as_u64()
        .ok_or_else(|| DowaiError::Config(format!("Effective {group}.{name} is invalid")))
}

fn resource_enforcement_mode(configuration: &Value) -> Result<&str> {
    configuration["effective"]["python"]["resource_enforcement"]
        .as_str()
        .ok_or_else(|| {
            DowaiError::Config("Effective python.resource_enforcement is invalid".to_owned())
        })
}

/// Resolve os limites de um worker antes de ele iniciar. Com `require`, recusa limites
/// solicitados que a plataforma não aplica.
pub(crate) fn worker_limits(configuration: &Value) -> Result<WorkerLimits> {
    let memory_mb = effective_u64(configuration, "runtime", "max_memory_mb")?;
    let cpu_percent = effective_u64(configuration, "runtime", "max_cpu_percent")?;
    let memory_state = memory_enforcement();
    if resource_enforcement_mode(configuration)? == "require" {
        let mut unsupported = Vec::new();
        if memory_state == Enforcement::NotEnforced {
            unsupported.push(format!("runtime.max_memory_mb={memory_mb}"));
        }
        if cpu_percent < 100 && cpu_enforcement() == Enforcement::NotEnforced {
            unsupported.push(format!("runtime.max_cpu_percent={cpu_percent}"));
        }
        if !unsupported.is_empty() {
            return Err(DowaiError::ResourceUnsupported(format!(
                "python.resource_enforcement=require: limites sem enforcement nesta plataforma: {}",
                unsupported.join(", ")
            )));
        }
    }
    Ok(WorkerLimits {
        memory_bytes: (memory_state != Enforcement::NotEnforced)
            .then(|| memory_mb.saturating_mul(1024 * 1024)),
    })
}

/// Avisos de execução (`RunResult.warnings`) sobre limites não aplicados ou parciais.
pub(crate) fn run_warnings(configuration: &Value, nodes: &[PlanNode]) -> Result<Vec<String>> {
    let memory_mb = effective_u64(configuration, "runtime", "max_memory_mb")?;
    let cpu_percent = effective_u64(configuration, "runtime", "max_cpu_percent")?;
    let max_parallel = effective_u64(configuration, "runtime", "max_parallel_nodes")?;
    let max_workers = effective_u64(configuration, "runtime", "max_worker_processes")?;
    let mut warnings = Vec::new();

    if nodes.iter().any(|node| crate::is_python_path(&node.path)) {
        let state = memory_enforcement();
        let detail = match state {
            Enforcement::Partial => {
                "aplicado a cada worker Python como RLIMIT_AS (memória virtual)"
            }
            Enforcement::NotEnforced => {
                "não aplicado nesta plataforma (Job Object pendente, ENT-093)"
            }
        };
        warnings.push(format!(
            "runtime.max_memory_mb={memory_mb} {detail}; estado {} (D06)",
            state.as_str()
        ));
    }
    if cpu_percent < 100 && cpu_enforcement() == Enforcement::NotEnforced {
        warnings.push(format!(
            "runtime.max_cpu_percent={cpu_percent} não aplicado; estado {} (D06)",
            cpu_enforcement().as_str()
        ));
    }
    // ENT-104: limites de threads e de bytes em voo ainda não são aplicados em nenhuma
    // plataforma. Cada um gera aviso `not_enforced` quando se aplica ao run.
    if !nodes.is_empty() {
        warnings.push(format!(
            "runtime.max_threads={} não aplicado; estado {} (ENT-104)",
            effective_u64(configuration, "runtime", "max_threads")?,
            Enforcement::NotEnforced.as_str()
        ));
    }
    if nodes.iter().any(|node| crate::is_python_path(&node.path)) {
        warnings.push(format!(
            "python.max_threads={} não aplicado; estado {} (ENT-104)",
            effective_u64(configuration, "python", "max_threads")?,
            Enforcement::NotEnforced.as_str()
        ));
        warnings.push(format!(
            "runtime.max_inflight_bytes={} não aplicado ao buffer de datasets Python (iter_rows é por linhas); estado {} (ENT-104)",
            effective_u64(configuration, "runtime", "max_inflight_bytes")?,
            Enforcement::NotEnforced.as_str()
        ));
    }
    if (max_parallel > 1 || max_workers > 1) && has_independent_nodes(nodes) {
        warnings.push(format!(
            "runtime.max_parallel_nodes={max_parallel} e runtime.max_worker_processes={max_workers}: o scheduler executa nós em sequência com um worker por vez (concorrência efetiva 1); limite respeitado, paralelismo entre nós não ativo"
        ));
    }
    Ok(warnings)
}

/// Para cada nó, o conjunto de índices de todas as dependências, diretas ou transitivas.
fn transitive_dependencies(nodes: &[PlanNode]) -> Vec<BTreeSet<usize>> {
    let index: BTreeMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(position, node)| (node.id.as_str(), position))
        .collect();
    (0..nodes.len())
        .map(|start| {
            let mut seen = BTreeSet::new();
            let mut pending: Vec<usize> = nodes[start]
                .depends_on
                .iter()
                .filter_map(|id| index.get(id.as_str()).copied())
                .collect();
            while let Some(current) = pending.pop() {
                if seen.insert(current) {
                    pending.extend(
                        nodes[current]
                            .depends_on
                            .iter()
                            .filter_map(|id| index.get(id.as_str()).copied()),
                    );
                }
            }
            seen
        })
        .collect()
}

/// Dois nós são independentes quando nenhum depende, direta ou transitivamente, do outro.
fn has_independent_nodes(nodes: &[PlanNode]) -> bool {
    let dependencies = transitive_dependencies(nodes);
    (0..nodes.len()).any(|first| {
        (first + 1..nodes.len()).any(|second| {
            !dependencies[first].contains(&second) && !dependencies[second].contains(&first)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn node(id: &str, path: &str, depends_on: &[&str]) -> PlanNode {
        PlanNode {
            id: id.to_owned(),
            path: path.to_owned(),
            depends_on: depends_on.iter().map(|value| (*value).to_owned()).collect(),
            sha256: String::new(),
        }
    }

    fn configuration(runtime: Value, python_mode: &str) -> Value {
        json!({
            "effective": {
                "runtime": runtime,
                "python": { "resource_enforcement": python_mode, "max_threads": 4 }
            }
        })
    }

    fn defaults() -> Value {
        json!({
            "max_parallel_nodes": 4,
            "max_worker_processes": 4,
            "max_memory_mb": 2048,
            "max_cpu_percent": 100,
            "max_threads": 8,
            "max_inflight_bytes": 134217728
        })
    }

    #[test]
    fn unenforced_thread_and_byte_limits_are_reported_for_python_runs() {
        let nodes = [node("a.py", "models/a.py", &[])];
        let warnings = run_warnings(&configuration(defaults(), "best_effort"), &nodes)
            .expect("warnings are computed from valid configuration");
        for needle in [
            "runtime.max_threads=8 não aplicado; estado not_enforced",
            "python.max_threads=4 não aplicado; estado not_enforced",
            "runtime.max_inflight_bytes=134217728 não aplicado",
        ] {
            assert!(
                warnings.iter().any(|w| w.contains(needle)),
                "missing warning {needle}: {warnings:?}"
            );
        }
    }

    #[test]
    fn python_only_limit_warnings_are_absent_for_sql_only_runs() {
        let nodes = [node("a.sql", "models/a.sql", &[])];
        let warnings = run_warnings(&configuration(defaults(), "best_effort"), &nodes)
            .expect("warnings are computed from valid configuration");
        assert!(warnings.iter().any(|w| w.contains("runtime.max_threads=8")));
        assert!(!warnings.iter().any(|w| w.contains("python.max_threads")));
        assert!(!warnings.iter().any(|w| w.contains("max_inflight_bytes")));
    }

    #[test]
    fn independent_nodes_are_detected_and_chains_are_not() {
        let chain = [
            node("a.py", "models/a.py", &[]),
            node("b.py", "models/b.py", &["a.py"]),
            node("c.py", "models/c.py", &["b.py"]),
        ];
        assert!(!has_independent_nodes(&chain));
        let fan = [
            node("a.py", "models/a.py", &[]),
            node("b.py", "models/b.py", &["a.py"]),
            node("d.py", "models/d.py", &[]),
        ];
        assert!(has_independent_nodes(&fan));
    }

    #[test]
    fn cyclic_metadata_does_not_loop_forever() {
        let cycle = [
            node("a.py", "models/a.py", &["b.py"]),
            node("b.py", "models/b.py", &["a.py"]),
        ];
        assert!(!has_independent_nodes(&cycle));
    }

    #[test]
    fn best_effort_never_refuses_and_sets_memory_limit_on_linux() {
        let limits = worker_limits(&configuration(defaults(), "best_effort"))
            .expect("best_effort never refuses");
        if cfg!(target_os = "linux") {
            assert_eq!(limits.memory_bytes, Some(2048 * 1024 * 1024));
        } else {
            assert_eq!(limits.memory_bytes, None);
        }
    }

    #[test]
    fn require_refuses_unenforced_cpu_percent() {
        let mut runtime = defaults();
        runtime["max_cpu_percent"] = json!(50);
        let error = worker_limits(&configuration(runtime, "require"))
            .expect_err("cpu percent is never enforced in this version");
        assert_eq!(error.code(), "DOWAI_RESOURCE_UNSUPPORTED");
        assert!(error.message().contains("runtime.max_cpu_percent=50"));
    }

    #[test]
    fn warnings_describe_unapplied_limits_and_sequential_scheduler() {
        let mut runtime = defaults();
        runtime["max_cpu_percent"] = json!(50);
        let nodes = [
            node("a.py", "models/a.py", &[]),
            node("d.py", "models/d.py", &[]),
        ];
        let warnings = run_warnings(&configuration(runtime, "best_effort"), &nodes)
            .expect("warnings are computed from valid configuration");
        assert!(warnings.iter().any(|w| w.contains("max_cpu_percent=50")));
        assert!(warnings
            .iter()
            .any(|w| w.contains("concorrência efetiva 1")));
    }

    #[test]
    fn no_parallelism_warning_for_a_linear_plan() {
        let nodes = [
            node("a.py", "models/a.py", &[]),
            node("b.py", "models/b.py", &["a.py"]),
        ];
        let warnings = run_warnings(&configuration(defaults(), "best_effort"), &nodes)
            .expect("warnings are computed from valid configuration");
        assert!(!warnings.iter().any(|w| w.contains("concorrência efetiva")));
    }
}
