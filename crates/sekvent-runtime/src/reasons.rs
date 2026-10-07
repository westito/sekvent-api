//! Machine-readable reasons ([`AppError::reason`](sekvent_error::AppError::reason))
//! of the errors this crate produces for jobs.

/// A trigger arrived while a run of the job was in progress here.
pub const JOB_ALREADY_RUNNING: &str = "JOB_ALREADY_RUNNING";
/// The job's guard answered that another instance holds the job.
pub const JOB_HELD_ELSEWHERE: &str = "JOB_HELD_ELSEWHERE";
/// The job's unit is not running: not started yet, draining or stopped.
pub const JOB_NOT_RUNNING: &str = "JOB_NOT_RUNNING";
/// The job's guard failed to answer.
pub const JOB_GUARD_FAILED: &str = "JOB_GUARD_FAILED";
/// The job's guard panicked; the call counts as a guard error.
pub const GUARD_PANICKED: &str = "GUARD_PANICKED";
/// A run took longer than the job's timeout and was dropped.
pub const JOB_TIMED_OUT: &str = "JOB_TIMED_OUT";
/// A run panicked.
pub const JOB_PANICKED: &str = "JOB_PANICKED";
/// The guard withdrew the right to run (a lost lease) and the run was dropped.
pub const LEASE_LOST: &str = "LEASE_LOST";
