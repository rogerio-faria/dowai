//! Retry policy for transient failures of SQL materializations (ENT-037, ADR 0005 R1–R5).
//!
//! Only managed `table`/`view` materializations are retried: each one runs in a single
//! transaction that rolls back on failure, so repeating it yields the same relation. Python
//! models and user-written SQL are never retried automatically (R2, item 7).

use dowai_core::{CancellationToken, DowaiError, Result};
use serde_json::Value;
use std::thread;
use std::time::{Duration, Instant};

/// Upper bound of one backoff interval (ADR 0005 R5). Not configurable yet.
pub(crate) const MAX_BACKOFF_MS: u64 = 60_000;

/// Granularity of the cancellation check while waiting between attempts.
const WAIT_SLICE: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RetryPolicy {
    /// Total attempts per node, including the first (R1). `1` disables retries.
    pub max_attempts: u32,
    pub backoff_ms: u64,
    pub jitter: bool,
}

impl RetryPolicy {
    /// Reads the effective `execution.retry_*` values resolved by the configuration.
    pub(crate) fn from_configuration(configuration: &Value) -> Result<Self> {
        let execution = &configuration["effective"]["execution"];
        let max_attempts = execution["retry_max_attempts"]
            .as_u64()
            .filter(|value| (1..=10).contains(value))
            .and_then(|value| u32::try_from(value).ok())
            .ok_or_else(|| {
                DowaiError::Config("Effective execution.retry_max_attempts is invalid".to_owned())
            })?;
        let backoff_ms = execution["retry_backoff_ms"]
            .as_u64()
            .filter(|value| *value <= 600_000)
            .ok_or_else(|| {
                DowaiError::Config("Effective execution.retry_backoff_ms is invalid".to_owned())
            })?;
        let jitter = execution["retry_jitter"].as_bool().ok_or_else(|| {
            DowaiError::Config("Effective execution.retry_jitter is invalid".to_owned())
        })?;
        Ok(Self {
            max_attempts,
            backoff_ms,
            jitter,
        })
    }
}

/// Delay before the attempt that follows the failed attempt `failed_attempt` (1-based).
///
/// Exponential from `backoff_ms`, capped at [`MAX_BACKOFF_MS`]. With jitter, the delay is
/// uniform in `[base / 2, base]`, chosen from `sample` (any value; the caller supplies it).
pub(crate) fn backoff_delay(policy: RetryPolicy, failed_attempt: u32, sample: u64) -> Duration {
    let exponent = failed_attempt.saturating_sub(1).min(32);
    let base = policy
        .backoff_ms
        .saturating_mul(1u64 << exponent)
        .min(MAX_BACKOFF_MS);
    let millis = if policy.jitter && base > 0 {
        let half = base / 2;
        half + sample % (half + 1)
    } else {
        base
    };
    Duration::from_millis(millis)
}

/// Runs `attempt` until it succeeds, a failure is not transient, the attempts are exhausted,
/// or `wait` reports an interruption (`false`). `attempt` receives the 1-based attempt number
/// and is responsible for journaling that attempt. Returns the last outcome.
pub(crate) fn run_with_retry<T, E>(
    policy: RetryPolicy,
    is_transient: impl Fn(&E) -> bool,
    mut attempt: impl FnMut(u32) -> std::result::Result<T, E>,
    mut wait: impl FnMut(Duration) -> bool,
    mut sample: impl FnMut() -> u64,
) -> std::result::Result<T, E> {
    let mut number = 1;
    loop {
        match attempt(number) {
            Ok(value) => return Ok(value),
            Err(error) => {
                if number >= policy.max_attempts || !is_transient(&error) {
                    return Err(error);
                }
                let delay = backoff_delay(policy, number, sample());
                if !wait(delay) {
                    return Err(error);
                }
                number += 1;
            }
        }
    }
}

/// Sleeps for `delay` in short slices. Returns `false` as soon as the run is cancelled.
pub(crate) fn wait_unless_cancelled(delay: Duration, cancel: &CancellationToken) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        if cancel.is_cancelled() {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        thread::sleep((deadline - now).min(WAIT_SLICE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;

    fn policy(max_attempts: u32, backoff_ms: u64, jitter: bool) -> RetryPolicy {
        RetryPolicy {
            max_attempts,
            backoff_ms,
            jitter,
        }
    }

    #[test]
    fn backoff_doubles_and_is_capped_without_jitter() {
        let p = policy(10, 100, false);
        assert_eq!(backoff_delay(p, 1, 0), Duration::from_millis(100));
        assert_eq!(backoff_delay(p, 2, 0), Duration::from_millis(200));
        assert_eq!(backoff_delay(p, 3, 0), Duration::from_millis(400));
        assert_eq!(
            backoff_delay(policy(10, 50_000, false), 3, 0),
            Duration::from_millis(MAX_BACKOFF_MS)
        );
        assert_eq!(backoff_delay(policy(10, 0, true), 4, 7), Duration::ZERO);
    }

    #[test]
    fn jitter_stays_between_half_and_full_base() {
        let p = policy(10, 1000, true);
        for sample in [0, 1, 499, 500, 501, 999, u64::MAX] {
            let millis = backoff_delay(p, 2, sample).as_millis();
            assert!((1000..=2000).contains(&millis), "{millis} out of range");
        }
    }

    #[test]
    fn transient_failure_recovers_and_every_attempt_is_reported() {
        let seen = RefCell::new(Vec::new());
        let waits = RefCell::new(Vec::new());
        let result: std::result::Result<&str, &str> = run_with_retry(
            policy(3, 10, false),
            |error: &&str| *error == "transient",
            |attempt| {
                seen.borrow_mut().push(attempt);
                if attempt < 3 {
                    Err("transient")
                } else {
                    Ok("done")
                }
            },
            |delay| {
                waits.borrow_mut().push(delay);
                true
            },
            || 0,
        );
        assert_eq!(result, Ok("done"));
        assert_eq!(*seen.borrow(), vec![1, 2, 3]);
        assert_eq!(
            *waits.borrow(),
            vec![Duration::from_millis(10), Duration::from_millis(20)]
        );
    }

    #[test]
    fn permanent_failure_is_not_repeated() {
        let attempts = RefCell::new(0);
        let result: std::result::Result<(), &str> = run_with_retry(
            policy(5, 10, false),
            |error: &&str| *error == "transient",
            |_| {
                *attempts.borrow_mut() += 1;
                Err("permanent")
            },
            |_| panic!("no wait expected for a permanent failure"),
            || 0,
        );
        assert_eq!(result, Err("permanent"));
        assert_eq!(*attempts.borrow(), 1);
    }

    #[test]
    fn exhausted_attempts_return_the_last_error() {
        let attempts = RefCell::new(0);
        let result: std::result::Result<(), u32> = run_with_retry(
            policy(3, 0, false),
            |_: &u32| true,
            |attempt| {
                *attempts.borrow_mut() += 1;
                Err(attempt)
            },
            |_| true,
            || 0,
        );
        assert_eq!(result, Err(3));
        assert_eq!(*attempts.borrow(), 3);
    }

    #[test]
    fn default_policy_never_retries() {
        let attempts = RefCell::new(0);
        let result: std::result::Result<(), &str> = run_with_retry(
            policy(1, 0, false),
            |_: &&str| true,
            |_| {
                *attempts.borrow_mut() += 1;
                Err("transient")
            },
            |_| panic!("max_attempts=1 must not wait"),
            || 0,
        );
        assert!(result.is_err());
        assert_eq!(*attempts.borrow(), 1);
    }

    #[test]
    fn interruption_during_wait_stops_retrying() {
        let attempts = RefCell::new(0);
        let result: std::result::Result<(), &str> = run_with_retry(
            policy(5, 10, false),
            |_: &&str| true,
            |_| {
                *attempts.borrow_mut() += 1;
                Err("transient")
            },
            |_| false,
            || 0,
        );
        assert_eq!(result, Err("transient"));
        assert_eq!(*attempts.borrow(), 1);
    }

    #[test]
    fn configuration_values_are_validated() {
        let config = json!({"effective": {"execution": {
            "retry_max_attempts": 3, "retry_backoff_ms": 250, "retry_jitter": false
        }}});
        assert_eq!(
            RetryPolicy::from_configuration(&config).expect("valid"),
            policy(3, 250, false)
        );
        let zero_attempts = json!({"effective": {"execution": {
            "retry_max_attempts": 0, "retry_backoff_ms": 0, "retry_jitter": false
        }}});
        assert!(RetryPolicy::from_configuration(&zero_attempts).is_err());
        let missing = json!({"effective": {"execution": {}}});
        assert!(RetryPolicy::from_configuration(&missing).is_err());
    }
}
