//! Node and outbox state machines (plan 5.5, ADR 0005).
//!
//! Every status persisted for a node or an external effect goes through these tables.
//! A transition that is not listed here is refused with `DowaiError::State`, so the
//! persisted history can only contain sequences the engine actually allows.

use crate::{DowaiError, Result};

/// Status of one node invocation (one attempt). Matches the journal's `invocations.status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeStatus {
    Pending,
    Ready,
    Running,
    Succeeded,
    Failed,
    Skipped,
    Cancelled,
    /// External effect whose result could not be confirmed. Never retried automatically.
    Uncertain,
}

impl NodeStatus {
    pub const ALL: [NodeStatus; 8] = [
        Self::Pending,
        Self::Ready,
        Self::Running,
        Self::Succeeded,
        Self::Failed,
        Self::Skipped,
        Self::Cancelled,
        Self::Uncertain,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Ready => "READY",
            Self::Running => "RUNNING",
            Self::Succeeded => "SUCCEEDED",
            Self::Failed => "FAILED",
            Self::Skipped => "SKIPPED",
            Self::Cancelled => "CANCELLED",
            Self::Uncertain => "UNCERTAIN",
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == text)
            .ok_or_else(|| DowaiError::State(format!("Unknown node status: {text}")))
    }

    /// `true` when no further transition is allowed for this attempt.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Skipped | Self::Cancelled | Self::Uncertain
        )
    }

    fn allows(self, next: NodeStatus) -> bool {
        use NodeStatus::*;
        matches!(
            (self, next),
            (Pending, Ready | Running | Skipped | Cancelled)
                | (Ready, Running | Skipped | Cancelled)
                | (Running, Succeeded | Failed | Cancelled | Uncertain)
        )
    }

    /// Validated transition. Returns `next` when allowed; otherwise a `DowaiError::State`.
    pub fn transition_to(self, next: NodeStatus) -> Result<NodeStatus> {
        if self.allows(next) {
            Ok(next)
        } else {
            Err(DowaiError::State(format!(
                "Invalid node status transition: {} -> {}",
                self.as_str(),
                next.as_str()
            )))
        }
    }
}

/// Status of one outbox entry for an external effect (ENT-051/ENT-062).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutboxStatus {
    Pending,
    Sent,
    Confirmed,
    Failed,
    Uncertain,
}

impl OutboxStatus {
    pub const ALL: [OutboxStatus; 5] = [
        Self::Pending,
        Self::Sent,
        Self::Confirmed,
        Self::Failed,
        Self::Uncertain,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Sent => "SENT",
            Self::Confirmed => "CONFIRMED",
            Self::Failed => "FAILED",
            Self::Uncertain => "UNCERTAIN",
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == text)
            .ok_or_else(|| DowaiError::State(format!("Unknown outbox status: {text}")))
    }

    fn allows(self, next: OutboxStatus) -> bool {
        use OutboxStatus::*;
        matches!(
            (self, next),
            (Pending, Sent)
                | (Sent, Confirmed | Failed | Uncertain)
                | (Uncertain, Confirmed | Failed)
        )
    }

    /// Validated transition. Returns `next` when allowed; otherwise a `DowaiError::State`.
    pub fn transition_to(self, next: OutboxStatus) -> Result<OutboxStatus> {
        if self.allows(next) {
            Ok(next)
        } else {
            Err(DowaiError::State(format!(
                "Invalid outbox status transition: {} -> {}",
                self.as_str(),
                next.as_str()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_statuses_round_trip_through_text() {
        for status in NodeStatus::ALL {
            assert_eq!(NodeStatus::parse(status.as_str()).expect("parse"), status);
        }
        assert!(NodeStatus::parse("BOGUS").is_err());
        assert!(NodeStatus::parse("succeeded").is_err());
    }

    #[test]
    fn running_attempt_reaches_each_allowed_final_status() {
        for next in [
            NodeStatus::Succeeded,
            NodeStatus::Failed,
            NodeStatus::Cancelled,
            NodeStatus::Uncertain,
        ] {
            assert_eq!(
                NodeStatus::Running.transition_to(next).expect("allowed"),
                next
            );
        }
    }

    #[test]
    fn pending_node_can_start_or_be_skipped_or_cancelled() {
        assert!(NodeStatus::Pending
            .transition_to(NodeStatus::Running)
            .is_ok());
        assert!(NodeStatus::Pending.transition_to(NodeStatus::Ready).is_ok());
        assert!(NodeStatus::Pending
            .transition_to(NodeStatus::Skipped)
            .is_ok());
        assert!(NodeStatus::Pending
            .transition_to(NodeStatus::Cancelled)
            .is_ok());
    }

    #[test]
    fn invalid_node_transitions_are_refused() {
        // Pending cannot finish without running.
        assert!(matches!(
            NodeStatus::Pending.transition_to(NodeStatus::Succeeded),
            Err(DowaiError::State(_))
        ));
        // Terminal states never move again.
        for terminal in [
            NodeStatus::Succeeded,
            NodeStatus::Failed,
            NodeStatus::Skipped,
            NodeStatus::Cancelled,
            NodeStatus::Uncertain,
        ] {
            assert!(terminal.is_terminal());
            for next in NodeStatus::ALL {
                assert!(
                    terminal.transition_to(next).is_err(),
                    "{} -> {} must be refused",
                    terminal.as_str(),
                    next.as_str()
                );
            }
        }
        // A running attempt cannot be reset to pending or ready.
        assert!(NodeStatus::Running
            .transition_to(NodeStatus::Pending)
            .is_err());
        assert!(NodeStatus::Running
            .transition_to(NodeStatus::Ready)
            .is_err());
        assert!(!NodeStatus::Running.is_terminal());
    }

    #[test]
    fn outbox_follows_send_then_confirm_or_uncertain() {
        assert!(OutboxStatus::Pending
            .transition_to(OutboxStatus::Sent)
            .is_ok());
        assert!(OutboxStatus::Sent
            .transition_to(OutboxStatus::Confirmed)
            .is_ok());
        assert!(OutboxStatus::Sent
            .transition_to(OutboxStatus::Uncertain)
            .is_ok());
        assert!(OutboxStatus::Uncertain
            .transition_to(OutboxStatus::Confirmed)
            .is_ok());
        assert!(OutboxStatus::Uncertain
            .transition_to(OutboxStatus::Failed)
            .is_ok());
    }

    #[test]
    fn invalid_outbox_transitions_are_refused() {
        // Nothing is confirmed without being sent first.
        assert!(OutboxStatus::Pending
            .transition_to(OutboxStatus::Confirmed)
            .is_err());
        // Uncertain cannot be silently resent: it only resolves to confirmed or failed.
        assert!(OutboxStatus::Uncertain
            .transition_to(OutboxStatus::Pending)
            .is_err());
        assert!(OutboxStatus::Confirmed
            .transition_to(OutboxStatus::Pending)
            .is_err());
        assert!(OutboxStatus::parse("DONE").is_err());
        for status in OutboxStatus::ALL {
            assert_eq!(OutboxStatus::parse(status.as_str()).expect("parse"), status);
        }
    }
}
