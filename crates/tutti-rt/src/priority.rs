//! [`request_priority`]: ask the OS to schedule the calling thread ahead of
//! ordinary ones, and say what it granted.
//!
//! The call is a request, not a promise. It only asks for a higher priority
//! within the OS's ordinary scheduler (on Linux `SCHED_OTHER`, so a lower nice
//! value), never a real-time class, and the OS may refuse it: Linux with
//! `EPERM`/`EACCES` below the process's `RLIMIT_NICE` (an unprivileged user,
//! a container, most CI). What each request means per OS is `wanted`'s table.
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
    /// policy: nice -20 on Linux, `HIGHEST` on Windows. For a thread with a
    /// deadline, such as a plugin's audio phase.
    Highest,
    /// A step above ordinary threads, never real time: about nice -10 on
    /// Linux, `ABOVE_NORMAL` on Windows, between the default and the maximum
    /// on macOS. For a thread that should keep ahead of the machine's
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

/// The cross-platform value (`0..=99`) `Raised` asks for on Linux and
/// Windows. thread-priority maps it by the platform's own rule: on Linux's
/// time-sharing policy a linear scale onto nice 19 to -20 (measured: 72 and
/// 73 are nice -10, 75 is -11); on Windows 61 to 79 is `ABOVE_NORMAL`.
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
const RAISED: u8 = 73;

/// What each request asks the OS for, per platform, as the `thread_priority`
/// value it is converted from (a pure function, so the mapping is tested
/// without the privilege to apply it).
///
/// | | `Highest` | `Raised` |
/// |---|---|---|
/// | Linux | nice -20 | nice -10 |
/// | Windows | `THREAD_PRIORITY_HIGHEST` | `THREAD_PRIORITY_ABOVE_NORMAL` |
/// | macOS, iOS | the `SCHED_OTHER` maximum | a quarter of the way down from it to the minimum |
///
/// macOS's time-sharing priority is a number in a small range (about 15 to
/// 47, default 31), so a cross-platform percentage would land outside it
/// and be refused; there the point is computed from
/// `sched_get_priority_min/max`.
fn wanted(request: PriorityRequest) -> thread_priority::ThreadPriority {
    match request {
        PriorityRequest::Highest => thread_priority::ThreadPriority::Max,
        PriorityRequest::Raised => raised(),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn raised() -> thread_priority::ThreadPriority {
    use thread_priority::{ThreadPriority, ThreadPriorityValue};
    // `RAISED` is in range, so the fallback is unreachable; `Max` is the
    // nearest request if it ever were not.
    ThreadPriorityValue::try_from(RAISED).map_or(ThreadPriority::Max, ThreadPriority::Crossplatform)
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn raised() -> thread_priority::ThreadPriority {
    use thread_priority::{
        NormalThreadSchedulePolicy, ThreadPriority, ThreadPriorityValue, ThreadSchedulePolicy,
    };
    let policy = ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Other);
    let (Ok(min), Ok(max)) = (
        ThreadPriority::min_value_for_policy(policy),
        ThreadPriority::max_value_for_policy(policy),
    ) else {
        return ThreadPriority::Max;
    };
    let point = max - (max - min) / 4;
    u8::try_from(point)
        .ok()
        .and_then(|p| ThreadPriorityValue::try_from(p).ok())
        .map_or(ThreadPriority::Max, ThreadPriority::Crossplatform)
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
    use thread_priority::set_current_thread_priority;
    match set_current_thread_priority(wanted(request)) {
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

#[cfg(test)]
mod mapping_tests {
    use super::*;

    /// The mapping, without privilege: what each request converts to for the
    /// time-sharing policy. Linux: nice -10 and -20.
    ///
    /// Mutations (run): `Raised` mapped to `ThreadPriority::Max` -> -20,
    /// fails; `RAISED` at 75 -> nice -11, fails; `Highest` mapped to
    /// `raised()` -> -10, fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_requests_map_to_nice_minus_10_and_minus_20() {
        use thread_priority::{NormalThreadSchedulePolicy, ThreadSchedulePolicy};
        let policy = ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Other);
        let nice = |r| wanted(r).to_posix(policy).unwrap();
        assert_eq!(nice(PriorityRequest::Raised), -10);
        assert_eq!(nice(PriorityRequest::Highest), -20);
    }

    /// Windows: `Raised` is `ABOVE_NORMAL`, `Highest` is `HIGHEST`. Compiled
    /// by `just check-windows`, run only on Windows.
    ///
    /// Mutation (not run here): `Raised` mapped to `Max` -> `Highest`, fails.
    #[cfg(windows)]
    #[test]
    fn the_requests_map_to_above_normal_and_highest() {
        use thread_priority::WinAPIThreadPriority;
        let win = |r| WinAPIThreadPriority::try_from(wanted(r)).unwrap();
        assert_eq!(
            win(PriorityRequest::Raised),
            WinAPIThreadPriority::AboveNormal
        );
        assert_eq!(win(PriorityRequest::Highest), WinAPIThreadPriority::Highest);
    }

    /// macOS: `Raised` lies strictly between the default-ish middle and the
    /// maximum of `SCHED_OTHER`, `Highest` is the maximum. Compiled only
    /// there; not run in this lane.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn raised_is_between_the_middle_and_the_maximum() {
        use thread_priority::{NormalThreadSchedulePolicy, ThreadPriority, ThreadSchedulePolicy};
        let policy = ThreadSchedulePolicy::Normal(NormalThreadSchedulePolicy::Other);
        let max = ThreadPriority::max_value_for_policy(policy).unwrap();
        let min = ThreadPriority::min_value_for_policy(policy).unwrap();
        let raised = wanted(PriorityRequest::Raised).to_posix(policy).unwrap();
        assert!((min + max) / 2 < raised && raised < max);
        assert_eq!(
            wanted(PriorityRequest::Highest).to_posix(policy).unwrap(),
            max
        );
    }
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
