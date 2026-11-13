//! Suíte de caos para estado e recuperação (ENT-067, plano §14 W13 e §15.1).
//!
//! Cada fase de execução e cada fase de envio de efeito externo recebe uma falha injetada.
//! "Morte do processo" é simulada de duas formas, sem tocar o banco diretamente:
//! - soltar o [`RunLease`] / [`ActivityGuard`] sem registrar o resultado (o que o SO faz
//!   com um processo encerrado: os locks são liberados e a marca `run_inflight` fica);
//! - interromper a chamada ao sink com `resume_unwind` (não aciona o hook de pânico, então
//!   o teste não polui a saída), deixando o efeito no estado que a transação anterior gravou.
//!
//! Fases cobertas:
//! - Execução: preparação (antes do commit), commit de variáveis, registro final, recuperação
//!   repetida, falha de commit por revisão obsoleta.
//! - Efeito externo: antes do claim (`PENDING`), após o claim sem evidência (`SENT`), após
//!   entrega sem confirmação, resposta incerta do receptor, claim concorrente e recuperação
//!   enquanto há trabalho vivo.
//!
//! Limitação registrada: não há kill real de processo filho nem interrupção no meio de uma
//! transação SQLite (ver AC013 em MATRIZ_REQUISITOS.md). Esses casos continuam PARTIAL.

use dowai_core::state::OutboxStatus;
use dowai_core::{
    create_project, DeliveryOutcome, EffectSink, NewRun, OutboxEntry, Probe, RunStart, RunStore,
    SessionStore, VariableOrigin, NOT_DELIVERED_CODE,
};
use serde_json::json;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::panic::{self, AssertUnwindSafe};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Infraestrutura
// ---------------------------------------------------------------------------

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: std::env::temp_dir().join(format!("dowai-chaos-{}", Uuid::new_v4())),
        }
    }
    fn journal(&self) -> RunStore {
        RunStore::new(self.dir.join("state/runs.sqlite3"))
    }
    fn sessions(&self) -> SessionStore {
        SessionStore::new(self.dir.join("state/sessions.sqlite3"))
    }
    fn project(&self) -> PathBuf {
        create_project(&self.dir, "chaos").expect("project")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn start(run_id: &str, session_id: Option<String>) -> RunStart {
    RunStart {
        run_id: run_id.to_owned(),
        session_id,
        project: "chaos".to_owned(),
        execution_mode: Some("automatic".to_owned()),
        started_at_ms: 1_000,
        requested_targets: Some(vec!["model.py".to_owned()]),
        profile: None,
        variable_count: Some(0),
        full_refresh: None,
    }
}

fn final_record(run_id: &str, status: &str) -> NewRun {
    NewRun {
        run_id: run_id.to_owned(),
        session_id: None,
        project: "chaos".to_owned(),
        status: status.to_owned(),
        execution_mode: Some("automatic".to_owned()),
        started_at_ms: 1_000,
        finished_at_ms: 1_010,
        duration_ms: 10,
        error_code: None,
        requested_targets: Some(vec!["model.py".to_owned()]),
        profile: None,
        variable_count: Some(0),
        full_refresh: None,
    }
}

fn status_of(journal: &RunStore, run_id: &str) -> String {
    journal.get(run_id).expect("get run").run.status
}

fn status_of_effect(store: &RunStore, key: &str) -> OutboxStatus {
    store
        .outbox_get(key)
        .expect("get effect")
        .expect("effect exists")
        .status
}

/// Receptor idempotente que representa o sistema externo. `applied` guarda cada efeito
/// realmente aplicado uma única vez; `deliveries` conta chamadas a `deliver`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Remote {
    /// Aplica e responde `Delivered`.
    Healthy,
    /// Aplica e morre (pânico) antes de devolver a resposta: entrega sem confirmação.
    AppliesThenDies,
    /// Morre (pânico) antes de aplicar: nada chegou ao receptor.
    DiesBeforeApplying,
    /// Responde `Unknown` sem aplicar: o desfecho é incerto.
    ReportsUnknown,
    /// Recusa com código: certamente não aplicado.
    Rejects,
}

#[derive(Clone, Default)]
struct RemoteState {
    applied: Rc<RefCell<BTreeSet<String>>>,
    deliveries: Rc<RefCell<usize>>,
}

struct Sink {
    mode: RefCell<Remote>,
    state: RemoteState,
}

impl Sink {
    fn new(mode: Remote, state: &RemoteState) -> Self {
        Self {
            mode: RefCell::new(mode),
            state: state.clone(),
        }
    }
    fn applied_count(&self) -> usize {
        self.state.applied.borrow().len()
    }
    fn deliveries(&self) -> usize {
        *self.state.deliveries.borrow()
    }
}

impl EffectSink for Sink {
    fn deliver(&self, entry: &OutboxEntry) -> DeliveryOutcome {
        *self.state.deliveries.borrow_mut() += 1;
        match *self.mode.borrow() {
            Remote::Healthy => {
                self.state
                    .applied
                    .borrow_mut()
                    .insert(entry.idempotency_key.clone());
                DeliveryOutcome::Delivered
            }
            Remote::AppliesThenDies => {
                self.state
                    .applied
                    .borrow_mut()
                    .insert(entry.idempotency_key.clone());
                panic::resume_unwind(Box::new("processo morto após aplicar o efeito"));
            }
            Remote::DiesBeforeApplying => {
                panic::resume_unwind(Box::new("processo morto antes de aplicar o efeito"));
            }
            Remote::ReportsUnknown => DeliveryOutcome::Unknown,
            Remote::Rejects => DeliveryOutcome::Rejected {
                error_code: "REMOTE_REJECTED".to_owned(),
            },
        }
    }

    fn probe(&self, entry: &OutboxEntry) -> Probe {
        if self.state.applied.borrow().contains(&entry.idempotency_key) {
            Probe::Delivered
        } else if *self.mode.borrow() == Remote::ReportsUnknown {
            Probe::Unknown
        } else {
            Probe::NotDelivered
        }
    }
}

/// Executa `dispatch` simulando morte do processo: pânico é capturado e ignorado, como
/// se o processo tivesse sido encerrado no meio do envio.
fn dispatch_until_death(store: &RunStore, sink: &Sink) {
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
        store.outbox_dispatch(sink).expect("dispatch");
    }));
    assert!(
        outcome.is_err(),
        "the injected fault must interrupt the send"
    );
}

// ---------------------------------------------------------------------------
// Fases de execução
// ---------------------------------------------------------------------------

#[test]
fn fase_preparacao_morte_antes_do_commit_nao_publica_variaveis() {
    let fixture = Fixture::new();
    let journal = fixture.journal();
    let sessions = fixture.sessions();
    let project = fixture.project();
    let session = sessions
        .create(&project, Duration::from_secs(3_600))
        .expect("session");
    let run_id = Uuid::new_v4().to_string();

    let lease = journal
        .begin_run(&start(&run_id, Some(session.session_id.clone())))
        .expect("begin");
    drop(lease); // morte antes de qualquer commit

    let report = journal.recover_interrupted(2_000).expect("recover");
    assert_eq!(report.interrupted_runs, vec![run_id.clone()]);
    assert_eq!(status_of(&journal, &run_id), "FAILED");
    assert!(sessions
        .snapshot(&session.session_id)
        .expect("snapshot")
        .is_empty());
}

#[test]
fn fase_commit_morte_apos_commit_preserva_checkpoint() {
    let fixture = Fixture::new();
    let journal = fixture.journal();
    let sessions = fixture.sessions();
    let project = fixture.project();
    let session = sessions
        .create(&project, Duration::from_secs(3_600))
        .expect("session");
    let base = sessions
        .snapshot_versioned(&session.session_id)
        .expect("base");
    let run_id = Uuid::new_v4().to_string();
    let lease = journal
        .begin_run(&start(&run_id, Some(session.session_id.clone())))
        .expect("begin");
    let origin = VariableOrigin {
        run_id: run_id.clone(),
        invocation_id: Uuid::new_v4().to_string(),
    };
    sessions
        .commit_variables(
            &session.session_id,
            &BTreeMap::from([("stage".to_owned(), json!("ready"))]),
            &base.revisions,
            &origin,
        )
        .expect("commit");
    drop(lease); // morte depois do commit e antes do registro

    journal.recover_interrupted(2_000).expect("recover");
    assert_eq!(status_of(&journal, &run_id), "FAILED");
    assert_eq!(
        sessions
            .snapshot(&session.session_id)
            .expect("snapshot")
            .get("stage"),
        Some(&json!("ready")),
        "the committed checkpoint must survive the crash"
    );
}

#[test]
fn fase_commit_com_revisao_obsoleta_nao_publica_nada() {
    let fixture = Fixture::new();
    let sessions = fixture.sessions();
    let project = fixture.project();
    let session = sessions
        .create(&project, Duration::from_secs(3_600))
        .expect("session");
    let stale = sessions
        .snapshot_versioned(&session.session_id)
        .expect("stale base");
    let origin = |run: &str| VariableOrigin {
        run_id: run.to_owned(),
        invocation_id: Uuid::new_v4().to_string(),
    };
    sessions
        .commit_variables(
            &session.session_id,
            &BTreeMap::from([("stage".to_owned(), json!("first"))]),
            &stale.revisions,
            &origin("run-a"),
        )
        .expect("first commit");

    let rejected = sessions.commit_variables(
        &session.session_id,
        &BTreeMap::from([("stage".to_owned(), json!("second"))]),
        &stale.revisions,
        &origin("run-b"),
    );
    assert!(rejected.is_err(), "a stale revision must not be published");
    assert_eq!(
        sessions
            .snapshot(&session.session_id)
            .expect("snapshot")
            .get("stage"),
        Some(&json!("first"))
    );
}

#[test]
fn fase_registro_morte_apos_registro_nao_gera_marca_nem_reexecucao() {
    let fixture = Fixture::new();
    let journal = fixture.journal();
    let run_id = Uuid::new_v4().to_string();
    let lease = journal.begin_run(&start(&run_id, None)).expect("begin");
    journal
        .record(&final_record(&run_id, "SUCCEEDED"), &[])
        .expect("record");
    drop(lease);

    let report = journal.recover_interrupted(5_000).expect("recover");
    assert!(report.interrupted_runs.is_empty());
    assert_eq!(status_of(&journal, &run_id), "SUCCEEDED");
}

#[test]
fn fase_recuperacao_repetida_e_idempotente() {
    let fixture = Fixture::new();
    let journal = fixture.journal();
    let run_id = Uuid::new_v4().to_string();
    drop(journal.begin_run(&start(&run_id, None)).expect("begin"));

    let first = journal.recover_interrupted(2_000).expect("first recovery");
    assert_eq!(first.interrupted_runs, vec![run_id.clone()]);
    // Simula queda da própria recuperação depois de gravar o resultado e antes de encerrar:
    // a segunda execução não pode registrar a mesma execução de novo.
    let second = journal.recover_interrupted(3_000).expect("second recovery");
    assert!(second.interrupted_runs.is_empty());
    assert_eq!(status_of(&journal, &run_id), "FAILED");
}

// ---------------------------------------------------------------------------
// Fases de envio de efeito externo
// ---------------------------------------------------------------------------

#[test]
fn efeito_morte_antes_do_claim_entrega_exatamente_uma_vez() {
    let fixture = Fixture::new();
    let store = fixture.journal();
    let remote = RemoteState::default();
    store
        .outbox_enqueue("mail:run-1:supervisor", "email", "supervisor@example.test")
        .expect("enqueue");
    // Morte antes do claim: a entrada continua PENDING e nada foi enviado.
    assert_eq!(
        status_of_effect(&store, "mail:run-1:supervisor"),
        OutboxStatus::Pending
    );

    let sink = Sink::new(Remote::Healthy, &remote);
    let report = store.outbox_dispatch(&sink).expect("dispatch");
    assert_eq!(report.confirmed, 1);
    store.outbox_dispatch(&sink).expect("second dispatch");
    assert_eq!(sink.deliveries(), 1, "one delivery across restarts");
    assert_eq!(sink.applied_count(), 1);
}

#[test]
fn efeito_morte_apos_claim_sem_evidencia_vira_uncertain_e_nao_reenvia() {
    let fixture = Fixture::new();
    let store = fixture.journal();
    let remote = RemoteState::default();
    store
        .outbox_enqueue("mail:run-2:supervisor", "email", "supervisor@example.test")
        .expect("enqueue");

    let sink = Sink::new(Remote::DiesBeforeApplying, &remote);
    dispatch_until_death(&store, &sink);
    assert_eq!(
        status_of_effect(&store, "mail:run-2:supervisor"),
        OutboxStatus::Sent,
        "after the claim, the crash leaves the entry SENT"
    );

    // Próxima recuperação marca UNCERTAIN; o receptor confirma que não aplicou.
    let report = store.recover_interrupted_for_test(2_000);
    assert_eq!(report, 1);
    store.outbox_dispatch(&sink).expect("reconcile");
    assert_eq!(
        status_of_effect(&store, "mail:run-2:supervisor"),
        OutboxStatus::Failed
    );
    assert_eq!(
        store
            .outbox_get("mail:run-2:supervisor")
            .expect("get")
            .expect("entry")
            .error_code
            .as_deref(),
        Some(NOT_DELIVERED_CODE)
    );
    // Nunca reenviado às cegas: a única entrega foi a que morreu antes de aplicar.
    assert_eq!(sink.deliveries(), 1);
    assert_eq!(sink.applied_count(), 0);
}

#[test]
fn efeito_morte_apos_entrega_sem_confirmacao_e_reconciliado_sem_duplicar() {
    let fixture = Fixture::new();
    let store = fixture.journal();
    let remote = RemoteState::default();
    store
        .outbox_enqueue("rest:run-3:post", "rest", "https://example.test/hook")
        .expect("enqueue");

    let sink = Sink::new(Remote::AppliesThenDies, &remote);
    dispatch_until_death(&store, &sink);
    assert_eq!(sink.applied_count(), 1, "the remote applied the effect");

    store.recover_interrupted_for_test(2_000);
    let sink = Sink::new(Remote::Healthy, &remote);
    store.outbox_dispatch(&sink).expect("reconcile");
    assert_eq!(
        status_of_effect(&store, "rest:run-3:post"),
        OutboxStatus::Confirmed
    );
    assert_eq!(
        sink.deliveries(),
        1,
        "only the original send; reconciliation probes, never resends"
    );
    assert_eq!(sink.applied_count(), 1, "no duplicate effect");
}

#[test]
fn efeito_resposta_incerta_permanece_uncertain_ate_haver_evidencia() {
    let fixture = Fixture::new();
    let store = fixture.journal();
    let remote = RemoteState::default();
    store
        .outbox_enqueue("mail:run-4:supervisor", "email", "supervisor@example.test")
        .expect("enqueue");

    let sink = Sink::new(Remote::ReportsUnknown, &remote);
    let report = store.outbox_dispatch(&sink).expect("dispatch");
    assert_eq!(report.uncertain, 1);
    assert_eq!(
        status_of_effect(&store, "mail:run-4:supervisor"),
        OutboxStatus::Uncertain
    );

    // Sem evidência, reconciliação mantém UNCERTAIN e não envia de novo.
    let report = store.outbox_dispatch(&sink).expect("reconcile");
    assert_eq!(report.uncertain, 1);
    assert_eq!(sink.deliveries(), 1, "UNCERTAIN is never resent blindly");
    assert_eq!(sink.applied_count(), 0);
}

#[test]
fn efeito_recusado_falha_sem_reenvio_automatico() {
    let fixture = Fixture::new();
    let store = fixture.journal();
    let remote = RemoteState::default();
    store
        .outbox_enqueue("mail:run-5:supervisor", "email", "supervisor@example.test")
        .expect("enqueue");

    let sink = Sink::new(Remote::Rejects, &remote);
    let report = store.outbox_dispatch(&sink).expect("dispatch");
    assert_eq!(report.failed, 1);
    store.outbox_dispatch(&sink).expect("again");
    assert_eq!(
        status_of_effect(&store, "mail:run-5:supervisor"),
        OutboxStatus::Failed
    );
    assert_eq!(sink.deliveries(), 1, "a refused effect is not resent");
}

#[test]
fn efeito_claim_concorrente_so_um_despachante_envia() {
    let fixture = Fixture::new();
    let store = fixture.journal();
    let other = fixture.journal();
    store
        .outbox_enqueue("mail:run-6:supervisor", "email", "supervisor@example.test")
        .expect("enqueue");

    assert!(store.outbox_claim("mail:run-6:supervisor").expect("first"));
    assert!(
        !other.outbox_claim("mail:run-6:supervisor").expect("second"),
        "a second dispatcher must not claim the same entry"
    );
}

#[test]
fn efeito_recuperacao_com_trabalho_vivo_adia_e_nao_marca_uncertain() {
    let fixture = Fixture::new();
    let store = fixture.journal();
    store
        .outbox_enqueue("mail:run-7:supervisor", "email", "supervisor@example.test")
        .expect("enqueue");
    assert!(store.outbox_claim("mail:run-7:supervisor").expect("claim"));

    // Outro processo está enviando: a guarda de atividade fica viva.
    let guard = store.activity_guard().expect("guard");
    let report = store
        .recover_interrupted(2_000)
        .expect("recover while live");
    assert!(report.outbox_deferred);
    assert_eq!(report.uncertain_effects, 0);
    assert_eq!(
        status_of_effect(&store, "mail:run-7:supervisor"),
        OutboxStatus::Sent
    );

    drop(guard);
    let report = store.recover_interrupted(3_000).expect("recover after");
    assert_eq!(report.uncertain_effects, 1);
    assert_eq!(
        status_of_effect(&store, "mail:run-7:supervisor"),
        OutboxStatus::Uncertain
    );
}

/// Atalho de teste: recuperação sem execuções interrompidas, devolvendo quantos efeitos
/// `SENT` viraram `UNCERTAIN`.
trait RecoverForTest {
    fn recover_interrupted_for_test(&self, now_ms: i64) -> usize;
}

impl RecoverForTest for RunStore {
    fn recover_interrupted_for_test(&self, now_ms: i64) -> usize {
        self.recover_interrupted(now_ms)
            .expect("recover")
            .uncertain_effects
    }
}
