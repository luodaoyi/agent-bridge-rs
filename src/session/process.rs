#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows_job;
#[cfg(windows)]
mod windows_launch;

#[cfg(unix)]
pub(super) use unix::*;
#[cfg(windows)]
pub(super) use windows_job::{WindowsJob, process_alive, terminate_windows_process};
#[cfg(windows)]
pub(crate) use windows_launch::run_windows_job_child;
#[cfg(windows)]
pub(super) use windows_launch::*;

use std::time::Duration;

use super::TERMINATION_GRACE_MIN_MS;

/// Highest Unix process-group signal already sent for this session.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum TerminationLevel {
    #[default]
    None,
    Hup,
    Term,
    Kill,
}

/// Result of a liveness probe of the owned process scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScopeAlive {
    /// The whole owned process scope is verifiably gone.
    Gone,
    /// The owned process scope is still alive.
    Alive,
    /// Liveness could not be verified (lock poisoned, or Windows could not
    /// open the process). Never treated as `Gone`.
    Unknown,
}

/// Session-level classification of the owned process scope for escalation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ScopeState {
    /// Session terminal or the whole owned scope verifiably gone.
    Gone,
    /// The owned scope is still alive.
    Alive,
    /// Cannot verify (probe inconclusive or no retained pid).
    Unknown,
}

pub(super) enum EscalationResult {
    /// The process is verified terminated (waiter observed its exit).
    Done,
    /// The absolute deadline passed with the process still live.
    Timeout,
    /// Sending the termination signal failed.
    Failed(String),
}

/// Escalation milestone inside a close budget: `fraction` of the budget, but
/// never sooner than the minimum grace so a graceful child is not
/// force-killed prematurely.
pub(super) fn escalation_milestone(
    budget: Duration,
    fraction_numerator: u64,
    fraction_denominator: u64,
) -> Duration {
    budget
        .mul_f64(fraction_numerator as f64 / fraction_denominator as f64)
        .max(Duration::from_millis(TERMINATION_GRACE_MIN_MS))
}
