//! Durable outbox for external effects (ENT-051, plan 9.3, AC017).
//!
//! An effect is first recorded as `PENDING` under an idempotency key, then claimed
//! (`PENDING -> SENT`) before it is delivered. Every state change is a single IMMEDIATE
//! transaction and is validated by [`OutboxStatus::transition_to`], so two dispatchers can
//! never claim the same entry.
//!
//! Recovery rules, which prevent duplicated effects:
//! - `PENDING` (crash before the claim): delivered once on the next dispatch.
//! - `SENT` (crash after the claim): not resent automatically. [`RunStore::outbox_recover_sent`]
//!   marks it `UNCERTAIN`, then the sink is asked whether the effect was delivered.
//! - `UNCERTAIN` with no evidence either way stays `UNCERTAIN`; it is never resent blindly.
//!
//! The journal stores the idempotency key, the effect kind, the target and the state, never
//! the payload. The sink rebuilds the payload from its own inputs, keyed by the same key.

use crate::runs::{bounded, current_time_ms, sanitize_code, state_error, RunStore};
use crate::state::OutboxStatus;
use crate::{DowaiError, Result};
use rusqlite::{params, OptionalExtension, TransactionBehavior};

const MAX_KEY_CHARS: usize = 128;
const MAX_KIND_CHARS: usize = 64;
/// Code recorded when a sink reports that an uncertain effect was not delivered.
pub const NOT_DELIVERED_CODE: &str = "DOWAI_OUTBOX_NOT_DELIVERED";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutboxEntry {
    pub idempotency_key: String,
    pub effect_kind: String,
    pub target: String,
    pub status: OutboxStatus,
    pub attempts: u32,
    pub error_code: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Result of one delivery attempt reported by the sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliveryOutcome {
    Delivered,
    /// The receiver refused the effect: it was certainly not applied.
    Rejected {
        error_code: String,
    },
    /// The outcome is unknown (for example, the connection dropped after sending).
    Unknown,
}

/// Answer of the receiver when asked about an effect whose outcome is uncertain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    Delivered,
    NotDelivered,
    Unknown,
}

/// Receiver of external effects. It must deduplicate by idempotency key.
pub trait EffectSink {
    fn deliver(&self, entry: &OutboxEntry) -> DeliveryOutcome;
    fn probe(&self, entry: &OutboxEntry) -> Probe;
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DispatchReport {
    pub confirmed: usize,
    pub failed: usize,
    pub uncertain: usize,
}

const ENTRY_COLUMNS: &str = "idempotency_key, effect_kind, target, status, attempts, \
                             error_code, created_at_ms, updated_at_ms";

impl RunStore {
    /// Records an effect as `PENDING`. Idempotent: enqueuing the same key and effect again
    /// returns the existing entry, whatever its state. The same key for a different effect
    /// is refused.
    pub fn outbox_enqueue(
        &self,
        idempotency_key: &str,
        effect_kind: &str,
        target: &str,
    ) -> Result<OutboxEntry> {
        validate_identifier("idempotency key", idempotency_key, MAX_KEY_CHARS)?;
        validate_identifier("effect kind", effect_kind, MAX_KIND_CHARS)?;
        let target = bounded(target);
        let now = current_time_ms();
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(state_error)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO outbox (idempotency_key, effect_kind, target, status,
                   attempts, error_code, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, 'PENDING', 0, NULL, ?4, ?4)",
                params![idempotency_key, effect_kind, target, now],
            )
            .map_err(state_error)?;
        let entry = read_entry(&transaction, idempotency_key)?
            .ok_or_else(|| DowaiError::State("Outbox entry vanished after insert".to_owned()))?;
        if entry.effect_kind != effect_kind || entry.target != target {
            return Err(DowaiError::State(
                "Idempotency key already registered for a different effect".to_owned(),
            ));
        }
        transaction.commit().map_err(state_error)?;
        Ok(entry)
    }

    /// Consultable state of one outbox entry.
    pub fn outbox_get(&self, idempotency_key: &str) -> Result<Option<OutboxEntry>> {
        let connection = self.connect()?;
        read_entry(&connection, idempotency_key)
    }

    /// Entries in one state, oldest first.
    pub fn outbox_list(&self, status: OutboxStatus) -> Result<Vec<OutboxEntry>> {
        let connection = self.connect()?;
        let mut statement = connection
            .prepare(&format!(
                "SELECT {ENTRY_COLUMNS} FROM outbox WHERE status = ?1 ORDER BY seq"
            ))
            .map_err(state_error)?;
        let rows = statement
            .query_map([status.as_str()], read_entry_row)
            .map_err(state_error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(state_error)
    }

    /// Atomically claims a `PENDING` entry (`PENDING -> SENT`). Returns `false` when the
    /// entry is in any other state, which is how a second dispatcher learns it must skip.
    pub fn outbox_claim(&self, idempotency_key: &str) -> Result<bool> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(state_error)?;
        let current = read_status(&transaction, idempotency_key)?;
        if current != OutboxStatus::Pending {
            return Ok(false);
        }
        OutboxStatus::Pending.transition_to(OutboxStatus::Sent)?;
        transaction
            .execute(
                "UPDATE outbox SET status = 'SENT', attempts = attempts + 1, updated_at_ms = ?2
                 WHERE idempotency_key = ?1",
                params![idempotency_key, current_time_ms()],
            )
            .map_err(state_error)?;
        transaction.commit().map_err(state_error)?;
        Ok(true)
    }

    /// Validated transition to `next`. Claiming (`-> SENT`) goes through
    /// [`RunStore::outbox_claim`] instead.
    pub fn outbox_transition(
        &self,
        idempotency_key: &str,
        next: OutboxStatus,
        error_code: Option<&str>,
    ) -> Result<OutboxEntry> {
        if next == OutboxStatus::Sent {
            return Err(DowaiError::State(
                "Use outbox_claim to move an entry to SENT".to_owned(),
            ));
        }
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(state_error)?;
        let current = read_status(&transaction, idempotency_key)?;
        let next = current.transition_to(next)?;
        transaction
            .execute(
                "UPDATE outbox SET status = ?2, error_code = ?3, updated_at_ms = ?4
                 WHERE idempotency_key = ?1",
                params![
                    idempotency_key,
                    next.as_str(),
                    sanitize_code(error_code),
                    current_time_ms()
                ],
            )
            .map_err(state_error)?;
        let entry = read_entry(&transaction, idempotency_key)?
            .ok_or_else(|| DowaiError::State("Outbox entry vanished".to_owned()))?;
        transaction.commit().map_err(state_error)?;
        Ok(entry)
    }

    /// Marks every `SENT` entry `UNCERTAIN`. Call it only when no other process is
    /// dispatching, typically at the start of a recovery. Returns how many entries changed.
    pub fn outbox_recover_sent(&self) -> Result<usize> {
        let mut connection = self.connect()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(state_error)?;
        OutboxStatus::Sent.transition_to(OutboxStatus::Uncertain)?;
        let changed = transaction
            .execute(
                "UPDATE outbox SET status = 'UNCERTAIN', error_code = 'DOWAI_OUTBOX_RECOVERED',
                   updated_at_ms = ?1 WHERE status = 'SENT'",
                params![current_time_ms()],
            )
            .map_err(state_error)?;
        transaction.commit().map_err(state_error)?;
        Ok(changed)
    }

    /// Reconciles `UNCERTAIN` entries through the sink, then delivers `PENDING` entries in
    /// order. An `UNCERTAIN` entry is never sent again: it is confirmed, failed, or kept.
    pub fn outbox_dispatch(&self, sink: &dyn EffectSink) -> Result<DispatchReport> {
        let mut report = DispatchReport::default();
        for entry in self.outbox_list(OutboxStatus::Uncertain)? {
            match sink.probe(&entry) {
                Probe::Delivered => {
                    self.outbox_transition(&entry.idempotency_key, OutboxStatus::Confirmed, None)?;
                    report.confirmed += 1;
                }
                Probe::NotDelivered => {
                    self.outbox_transition(
                        &entry.idempotency_key,
                        OutboxStatus::Failed,
                        Some(NOT_DELIVERED_CODE),
                    )?;
                    report.failed += 1;
                }
                Probe::Unknown => report.uncertain += 1,
            }
        }
        for entry in self.outbox_list(OutboxStatus::Pending)? {
            if !self.outbox_claim(&entry.idempotency_key)? {
                continue;
            }
            let claimed = OutboxEntry {
                status: OutboxStatus::Sent,
                ..entry
            };
            match sink.deliver(&claimed) {
                DeliveryOutcome::Delivered => {
                    self.outbox_transition(
                        &claimed.idempotency_key,
                        OutboxStatus::Confirmed,
                        None,
                    )?;
                    report.confirmed += 1;
                }
                DeliveryOutcome::Rejected { error_code } => {
                    self.outbox_transition(
                        &claimed.idempotency_key,
                        OutboxStatus::Failed,
                        Some(&error_code),
                    )?;
                    report.failed += 1;
                }
                DeliveryOutcome::Unknown => {
                    self.outbox_transition(
                        &claimed.idempotency_key,
                        OutboxStatus::Uncertain,
                        Some("DOWAI_OUTBOX_UNKNOWN_OUTCOME"),
                    )?;
                    report.uncertain += 1;
                }
            }
        }
        Ok(report)
    }
}

fn validate_identifier(label: &str, value: &str, max_chars: usize) -> Result<()> {
    let valid = !value.is_empty()
        && value.chars().count() <= max_chars
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-' | b'/')
        });
    if valid {
        Ok(())
    } else {
        Err(DowaiError::Config(format!(
            "Outbox {label} must have 1 to {max_chars} characters from [A-Za-z0-9._:/-]"
        )))
    }
}

fn read_status(connection: &rusqlite::Connection, idempotency_key: &str) -> Result<OutboxStatus> {
    let text: Option<String> = connection
        .query_row(
            "SELECT status FROM outbox WHERE idempotency_key = ?1",
            [idempotency_key],
            |row| row.get(0),
        )
        .optional()
        .map_err(state_error)?;
    let text = text
        .ok_or_else(|| DowaiError::State("No outbox entry with this idempotency key".to_owned()))?;
    OutboxStatus::parse(&text)
}

fn read_entry(
    connection: &rusqlite::Connection,
    idempotency_key: &str,
) -> Result<Option<OutboxEntry>> {
    connection
        .query_row(
            &format!("SELECT {ENTRY_COLUMNS} FROM outbox WHERE idempotency_key = ?1"),
            [idempotency_key],
            read_entry_row,
        )
        .optional()
        .map_err(state_error)
}

fn read_entry_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<OutboxEntry> {
    let status: String = row.get(3)?;
    let status = OutboxStatus::parse(&status).map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            "unknown outbox status".into(),
        )
    })?;
    Ok(OutboxEntry {
        idempotency_key: row.get(0)?,
        effect_kind: row.get(1)?,
        target: row.get(2)?,
        status,
        attempts: u32::try_from(row.get::<_, i64>(4)?).unwrap_or(u32::MAX),
        error_code: row.get(5)?,
        created_at_ms: row.get(6)?,
        updated_at_ms: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::PathBuf;
    use uuid::Uuid;

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!("dowai-outbox-test-{}", Uuid::new_v4()));
            Self { dir }
        }
        fn store(&self) -> RunStore {
            RunStore::new(self.dir.join("state/runs.sqlite3"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    /// Idempotent receiver: `effects` holds each applied key once; `deliveries` counts calls.
    #[derive(Default)]
    struct Receiver {
        effects: RefCell<BTreeSet<String>>,
        deliveries: RefCell<usize>,
        reject: Option<&'static str>,
        unknown_on_deliver: bool,
    }

    impl EffectSink for Receiver {
        fn deliver(&self, entry: &OutboxEntry) -> DeliveryOutcome {
            *self.deliveries.borrow_mut() += 1;
            if let Some(code) = self.reject {
                return DeliveryOutcome::Rejected {
                    error_code: code.to_owned(),
                };
            }
            if self.unknown_on_deliver {
                return DeliveryOutcome::Unknown;
            }
            self.effects
                .borrow_mut()
                .insert(entry.idempotency_key.clone());
            DeliveryOutcome::Delivered
        }
        fn probe(&self, entry: &OutboxEntry) -> Probe {
            if self.effects.borrow().contains(&entry.idempotency_key) {
                Probe::Delivered
            } else {
                Probe::NotDelivered
            }
        }
    }

    #[test]
    fn enqueue_is_idempotent_and_refuses_a_key_for_another_effect() {
        let fixture = Fixture::new();
        let store = fixture.store();
        let first = store
            .outbox_enqueue("alert:run-1:email", "email", "supervisor@example.test")
            .expect("enqueue");
        assert_eq!(first.status, OutboxStatus::Pending);
        let again = store
            .outbox_enqueue("alert:run-1:email", "email", "supervisor@example.test")
            .expect("re-enqueue");
        assert_eq!(again.created_at_ms, first.created_at_ms);
        assert!(matches!(
            store.outbox_enqueue("alert:run-1:email", "rest", "https://x.test"),
            Err(DowaiError::State(_))
        ));
        assert!(matches!(
            store.outbox_enqueue("has space", "email", "t"),
            Err(DowaiError::Config(_))
        ));
    }

    #[test]
    fn crash_between_enqueue_and_send_delivers_the_effect_once() {
        let fixture = Fixture::new();
        fixture
            .store()
            .outbox_enqueue("k-1", "email", "gerente")
            .expect("enqueue");
        // Process "crashes" here: the entry is persisted, nothing was sent.
        let store = fixture.store();
        let receiver = Receiver::default();
        let report = store.outbox_dispatch(&receiver).expect("dispatch");
        assert_eq!(report.confirmed, 1);
        assert_eq!(receiver.effects.borrow().len(), 1);
        assert_eq!(
            store.outbox_get("k-1").expect("get").expect("entry").status,
            OutboxStatus::Confirmed
        );
        // A second dispatch must not send again.
        let second = store.outbox_dispatch(&receiver).expect("dispatch again");
        assert_eq!(second, DispatchReport::default());
        assert_eq!(*receiver.deliveries.borrow(), 1);
    }

    #[test]
    fn crash_after_delivery_before_confirmation_is_reconciled_without_resending() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store
            .outbox_enqueue("k-2", "rest", "https://hooks.test/a")
            .expect("enqueue");
        let receiver = Receiver::default();
        // Claimed and delivered, but the process died before recording the confirmation.
        assert!(store.outbox_claim("k-2").expect("claim"));
        let entry = store.outbox_get("k-2").expect("get").expect("entry");
        assert_eq!(receiver.deliver(&entry), DeliveryOutcome::Delivered);
        drop(store);

        let store = fixture.store();
        assert_eq!(store.outbox_recover_sent().expect("recover"), 1);
        assert_eq!(
            store.outbox_get("k-2").expect("get").expect("entry").status,
            OutboxStatus::Uncertain
        );
        let report = store.outbox_dispatch(&receiver).expect("dispatch");
        assert_eq!(report.confirmed, 1);
        assert_eq!(receiver.effects.borrow().len(), 1);
        assert_eq!(*receiver.deliveries.borrow(), 1, "no second delivery");
    }

    #[test]
    fn uncertain_without_evidence_is_never_resent() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.outbox_enqueue("k-3", "email", "x").expect("enqueue");
        assert!(store.outbox_claim("k-3").expect("claim"));
        store.outbox_recover_sent().expect("recover");
        let receiver = Receiver {
            unknown_on_deliver: true,
            ..Receiver::default()
        };
        struct Silent;
        impl EffectSink for Silent {
            fn deliver(&self, _: &OutboxEntry) -> DeliveryOutcome {
                panic!("uncertain entry must not be delivered again");
            }
            fn probe(&self, _: &OutboxEntry) -> Probe {
                Probe::Unknown
            }
        }
        let report = store.outbox_dispatch(&Silent).expect("dispatch");
        assert_eq!(report.uncertain, 1);
        assert_eq!(
            store.outbox_get("k-3").expect("get").expect("entry").status,
            OutboxStatus::Uncertain
        );
        assert_eq!(*receiver.deliveries.borrow(), 0);
    }

    #[test]
    fn unknown_outcome_on_first_send_is_uncertain_and_not_retried() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.outbox_enqueue("k-4", "rest", "u").expect("enqueue");
        let receiver = Receiver {
            unknown_on_deliver: true,
            ..Receiver::default()
        };
        let report = store.outbox_dispatch(&receiver).expect("dispatch");
        assert_eq!(report.uncertain, 1);
        let entry = store.outbox_get("k-4").expect("get").expect("entry");
        assert_eq!(entry.status, OutboxStatus::Uncertain);
        assert_eq!(entry.attempts, 1);
        assert_eq!(
            entry.error_code.as_deref(),
            Some("DOWAI_OUTBOX_UNKNOWN_OUTCOME")
        );
        store.outbox_dispatch(&receiver).expect("dispatch again");
        assert_eq!(*receiver.deliveries.borrow(), 1);
    }

    #[test]
    fn rejected_delivery_is_failed_with_a_sanitized_code() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.outbox_enqueue("k-5", "email", "x").expect("enqueue");
        let receiver = Receiver {
            reject: Some("DOWAI_SMTP_REJECTED secret=abc"),
            ..Receiver::default()
        };
        let report = store.outbox_dispatch(&receiver).expect("dispatch");
        assert_eq!(report.failed, 1);
        let entry = store.outbox_get("k-5").expect("get").expect("entry");
        assert_eq!(entry.status, OutboxStatus::Failed);
        assert_eq!(entry.error_code.as_deref(), Some("DOWAI_UNKNOWN"));
    }

    #[test]
    fn invalid_transitions_and_claims_are_refused() {
        let fixture = Fixture::new();
        let store = fixture.store();
        store.outbox_enqueue("k-6", "email", "x").expect("enqueue");
        assert!(matches!(
            store.outbox_transition("k-6", OutboxStatus::Confirmed, None),
            Err(DowaiError::State(_))
        ));
        assert!(matches!(
            store.outbox_transition("k-6", OutboxStatus::Sent, None),
            Err(DowaiError::State(_))
        ));
        assert!(store.outbox_claim("k-6").expect("claim"));
        assert!(
            !store.outbox_claim("k-6").expect("second claim"),
            "one claim only"
        );
        store
            .outbox_transition("k-6", OutboxStatus::Confirmed, None)
            .expect("confirm");
        assert!(matches!(
            store.outbox_transition("k-6", OutboxStatus::Failed, None),
            Err(DowaiError::State(_))
        ));
        assert!(matches!(
            store.outbox_transition("missing", OutboxStatus::Confirmed, None),
            Err(DowaiError::State(_))
        ));
        assert_eq!(
            store
                .outbox_list(OutboxStatus::Confirmed)
                .expect("list")
                .len(),
            1
        );
    }
}
