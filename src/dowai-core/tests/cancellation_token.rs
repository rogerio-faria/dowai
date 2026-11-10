//! Cancellation token (plan 5.5: Ctrl+C/SIGTERM/IPC cancellation) and its stable error.

use dowai_core::{CancellationToken, DowaiError};
use std::thread;
use std::time::Duration;

#[test]
fn token_starts_uncancelled_and_cancel_is_idempotent() {
    let token = CancellationToken::new();
    assert!(!token.is_cancelled());
    token.cancel();
    token.cancel();
    assert!(token.is_cancelled());
}

#[test]
fn clones_share_state_across_threads() {
    let token = CancellationToken::new();
    let observer = token.clone();
    let handle = thread::spawn(move || {
        while !observer.is_cancelled() {
            thread::sleep(Duration::from_millis(1));
        }
        true
    });
    thread::sleep(Duration::from_millis(20));
    token.cancel();
    assert!(handle.join().expect("observer thread"));
}

#[test]
fn independent_tokens_do_not_affect_each_other() {
    let first = CancellationToken::new();
    let second = CancellationToken::new();
    first.cancel();
    assert!(!second.is_cancelled());
}

#[test]
fn check_returns_the_stable_cancelled_error() {
    let token = CancellationToken::new();
    assert!(token.check().is_ok());
    token.cancel();
    let error = token.check().expect_err("cancelled token must fail");
    assert!(matches!(error, DowaiError::Cancelled(_)));
    assert_eq!(error.code(), "DOWAI_CANCELLED");
    assert_eq!(error.exit_code(), 130);
}
