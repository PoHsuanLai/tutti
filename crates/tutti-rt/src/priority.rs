//! [`request_priority`]: ask the OS to schedule the calling thread ahead of
//! ordinary ones, and say what it granted.
//!
//! The call is a request, not a promise. On Linux the time-sharing policy
//! (`SCHED_OTHER`) is the only one this asks for, so the best it can be is a
//! lower nice value: never real time, and refused with `EPERM`/`EACCES` below
//! the process's `RLIMIT_NICE` (an unprivileged user, a container, most CI).
//! The result is read back as a [`PriorityOutcome`], so a caller logs what it got,
//! rather than the name of what it asked for.
//!
//! This is control-side code, run once when a thread starts; it allocates
//! (the refusal's message) and is not for the audio callback.

use std::fmt;

/// What a thread asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PriorityRequest {
    /// The highest priority the scheduler grants the thread's current
    /// policy: nice -20 on Linux's time-sharing policy. For a thread with a
    /// deadline, such as a plugin's audio phase.
    Highest,
    /// A step above ordinary threads, never real time: about nice -10 on
    /// Linux. For a thread that should keep ahead of the machine's
    /// background work but not of the audio threads, such as disk streaming.
    Raised,
}

/// The scheduling class the thread ended up in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scheduling {
    /// A real-time policy (`SCHED_FIFO`/`SCHED_RR`): the thread already was
    /// one, and kept it. This call never moves a thread into one.
    Realtime,
    /// The OS's ordinary time-sharing scheduler, with its priority raised.
    TimeSharing,
}

/// What [`request_priority`] got. `Display` is the line to log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PriorityOutcome {
    /// The OS accepted the request.
    Granted {
        /// What was asked for.
        requested: PriorityRequest,
        /// The class the thread is scheduled in.
        scheduling: Scheduling,
    },
    /// The OS refused; the thread's priority is unchanged.
    Refused {
        /// What was asked for.
        requested: PriorityRequest,
        /// The OS error, as text.
        error: String,
    },
}

impl PriorityOutcome {
    /// Whether the OS accepted the request.
    #[must_use]
    pub fn is_granted(&self) -> bool {
        matches!(self, PriorityOutcome::Granted { .. })
    }
}

impl fmt::Display for PriorityOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PriorityOutcome::Granted {
                requested,
                scheduling,
            } => write!(
                f,
                "thread priority {requested:?} granted, scheduled {}",
                match scheduling {
                    Scheduling::Realtime => "real time",
                    Scheduling::TimeSharing => "time-sharing, not real time",
                }
            ),
            PriorityOutcome::Refused { requested, error } => write!(
                f,
                "thread priority {requested:?} refused ({error}); the thread keeps its priority"
            ),
        }
    }
}

/// Ask the OS to raise the **calling thread**'s priority, and report what it
/// did. A refusal changes nothing and is harmless to the caller.
///
/// ```
/// use tutti_rt::{PriorityRequest, request_priority};
///
/// // Unprivileged, this is a refusal; privileged, a grant. Either is an answer.
/// let outcome = request_priority(PriorityRequest::Raised);
/// println!("{outcome}");
/// ```
#[must_use]
pub fn request_priority(request: PriorityRequest) -> PriorityOutcome {
    use thread_priority::{ThreadPriority, ThreadPriorityValue, set_current_thread_priority};
    let wanted = match request {
        PriorityRequest::Highest => ThreadPriority::Max,
        // Cross-platform 0..=100 (100 highest). On Linux's time-sharing
        // policy the crate maps it linearly onto nice 19..=-20; 75 is -10.
        PriorityRequest::Raised => match ThreadPriorityValue::try_from(75u8) {
            Ok(v) => ThreadPriority::Crossplatform(v),
            Err(_) => ThreadPriority::Max,
        },
    };
    match set_current_thread_priority(wanted) {
        Ok(()) => PriorityOutcome::Granted {
            requested: request,
            scheduling: scheduling(),
        },
        Err(e) => PriorityOutcome::Refused {
            requested: request,
            error: format!("{e:?}"),
        },
    }
}

#[cfg(unix)]
fn scheduling() -> Scheduling {
    match thread_priority::thread_schedule_policy() {
        Ok(thread_priority::ThreadSchedulePolicy::Realtime(_)) => Scheduling::Realtime,
        _ => Scheduling::TimeSharing,
    }
}

#[cfg(not(unix))]
fn scheduling() -> Scheduling {
    Scheduling::TimeSharing
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// The nice value of the calling thread, from `/proc/thread-self/stat`
    /// (field 19; `comm` is parenthesised and may hold spaces).
    fn nice() -> i64 {
        let stat = std::fs::read_to_string("/proc/thread-self/stat").unwrap();
        let rest = &stat[stat.rfind(')').unwrap() + 2..];
        // `rest` starts at field 3 (state).
        rest.split_whitespace()
            .nth(19 - 3)
            .unwrap()
            .parse()
            .unwrap()
    }

    fn on_a_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        std::thread::spawn(f).join().unwrap()
    }

    /// The outcome agrees with the OS: a grant of `Raised` leaves the thread
    /// at nice -10 and time-sharing; a refusal leaves its nice untouched.
    /// Unprivileged runs see the refusal arm, privileged ones the grant.
    ///
    /// Mutation (run, unprivileged: `RLIMIT_NICE` 0, so the OS refuses):
    /// the `Err` arm reported as `Granted` -> the grant arm sees nice
    /// unchanged at 0, not -10, and fails. Not run, because they need
    /// `CAP_SYS_NICE` to reach the grant arm (the test passes by the refusal
    /// arm without it): `Raised` mapped to `ThreadPriority::Max` -> nice -20,
    /// fails; the `Ok` arm reported as `Refused` -> fails.
    #[test]
    fn raised_reports_what_the_os_did() {
        let (outcome, before, after) = on_a_thread(|| {
            let before = nice();
            let outcome = request_priority(PriorityRequest::Raised);
            (outcome, before, nice())
        });
        match outcome {
            PriorityOutcome::Granted {
                requested,
                scheduling,
            } => {
                assert_eq!(requested, PriorityRequest::Raised);
                assert_eq!(scheduling, Scheduling::TimeSharing, "never real time");
                assert_eq!(after, -10);
            }
            PriorityOutcome::Refused { requested, .. } => {
                assert_eq!(requested, PriorityRequest::Raised);
                assert_eq!(after, before, "a refusal changes nothing");
            }
        }
    }

    /// `Highest` on a time-sharing thread is nice -20 when granted, and is
    /// never reported as real time.
    ///
    /// Mutation (not run here, needs `CAP_SYS_NICE`): `Highest` mapped to
    /// `Crossplatform(75)` -> nice -10, fails the -20 check. The unprivileged
    /// refusal arm checks only that the nice value did not move.
    #[test]
    fn highest_is_nice_minus_20_not_real_time() {
        let (outcome, before, after) = on_a_thread(|| {
            let before = nice();
            (request_priority(PriorityRequest::Highest), before, nice())
        });
        match outcome {
            PriorityOutcome::Granted { scheduling, .. } => {
                assert_eq!(scheduling, Scheduling::TimeSharing);
                assert_eq!(after, -20);
            }
            PriorityOutcome::Refused { .. } => assert_eq!(after, before),
        }
        assert!(!outcome.to_string().is_empty());
    }
}
