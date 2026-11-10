//! Cooperative cancellation shared by the CLI (signals), the runtime (node loop, Python
//! supervision, SQL watcher) and any future IPC/API entry point (plan 5.5).

use crate::{DowaiError, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Cheap, cloneable flag. Cancelling is idempotent and visible to every clone.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Err(DowaiError::Cancelled)` once cancelled; lets callers use `?` between steps.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() {
            Err(DowaiError::cancelled())
        } else {
            Ok(())
        }
    }
}
