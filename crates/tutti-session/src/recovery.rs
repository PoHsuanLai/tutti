//! The policy for a plugin that died or hung: a pure state machine
//! ([`Recovery::step`]) with time as a value and no thread, no I/O and no
//! plugin in it.
//!
//! It only decides. The session (`plugin.rs`) feeds it what it observed and
//! carries out the [`RecoveryAction`]: it loads the replacement through the existing
//! [`PluginLoad`](crate::PluginLoad) job, ends a hung server, or removes the
//! node. The snapshot bytes stay with the session's record, so the core never
//! holds them.
//!
//! # The budget
//!
//! At most [`RecoveryPolicy::max_failures`] failures (a death, or a reload that
//! did not come up) within [`RecoveryPolicy::window`]. A plugin that crashes on
//! load fails again and again inside the window and is given up on; one that
//! runs for longer than the window between crashes never is.
//!
//! A hang is not recovery's to detect: the session's
//! `HangDetector` ends a hung server with or without a
//! policy, and the death that follows is a failure here like any other.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tutti_plugin_types::{BridgeError, PluginFailure};

use crate::CrossfadeRefused;

/// When to reload a plugin and when to give up. Times are measured on the
/// `now` each [`Recovery::step`] is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryPolicy {
    /// Failures tolerated within [`window`](Self::window); one more gives up.
    pub max_failures: u32,
    /// How far back a failure counts against the budget.
    pub window: Duration,
    /// The wait before the first reload; it doubles with each failure in the
    /// window.
    pub backoff: Duration,
    /// The longest wait between attempts.
    pub backoff_cap: Duration,
    /// How long after a plugin restores a snapshot a death is taken to be
    /// the snapshot's doing: the next load goes without it
    /// ([`StateRestore::Withheld`](crate::StateRestore::Withheld)).
    pub state_probation: Duration,
    /// How long a reload may run before it counts as failed.
    pub load_timeout: Duration,
}

impl Default for RecoveryPolicy {
    /// Three failures in a minute, waits of 0.25 s doubling to 8 s, and a
    /// minute to load.
    fn default() -> Self {
        Self {
            max_failures: 3,
            window: Duration::from_secs(60),
            backoff: Duration::from_millis(250),
            backoff_cap: Duration::from_secs(8),
            state_probation: Duration::from_secs(10),
            load_timeout: Duration::from_secs(60),
        }
    }
}

/// What the session observed.
#[derive(Debug, Clone)]
pub enum RecoveryEvent {
    /// The plugin was declared dead, with the engine's latched cause.
    Died {
        /// Why.
        cause: PluginFailure,
    },
    /// The reload came up and is in the graph.
    Loaded,
    /// The reload did not come up.
    LoadFailed {
        /// Why.
        cause: RecoveryFailure,
    },
    /// Time passed with nothing else to report.
    Tick,
}

/// What the session does next.
#[derive(Debug, Clone)]
pub enum RecoveryAction {
    /// Nothing.
    Wait,
    /// Load a replacement, restoring the last snapshot if there is one,
    /// after `delay`.
    Reload {
        /// How long to wait first.
        delay: Duration,
        /// How many failures this makes in the window.
        attempt: u32,
    },
    /// Stop trying: remove the node.
    GiveUp(GiveUp),
}

/// Why recovery stopped.
#[derive(Debug, Clone)]
pub struct GiveUp {
    /// Failures within the window, including this one.
    pub failures: u32,
    /// The window they fell in.
    pub window: Duration,
    /// The last failure's cause: a death, or a reload that did not come up.
    pub last: ReloadCause,
}

impl std::fmt::Display for GiveUp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} failures within {:?}; the last: {}",
            self.failures, self.window, self.last
        )
    }
}

/// Why a plugin needs a replacement: what a reload follows.
///
/// The failure behind is its [`source`](std::error::Error::source).
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum ReloadCause {
    /// The plugin died or was ended: the cause the engine latched.
    #[error("{0}")]
    Died(#[from] PluginFailure),
    /// The attempt to replace it failed, and another follows.
    #[error("{0}")]
    Attempt(#[from] RecoveryFailure),
}

/// How many ports a node has, for a [`RecoveryFailure::ReplacementPorts`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortCounts {
    /// Audio inputs.
    pub audio_in: usize,
    /// Audio outputs.
    pub audio_out: usize,
    /// Event inputs.
    pub event_in: usize,
    /// Event outputs.
    pub event_out: usize,
}

impl std::fmt::Display for PortCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} audio in, {} out, {} event in, {} out",
            self.audio_in, self.audio_out, self.event_in, self.event_out
        )
    }
}

/// Why an attempt to replace a dead plugin failed (the session's reload, not
/// the plugin's death: that is [`PluginFailure`]).
///
/// An error behind a failure is its [`source`](std::error::Error::source),
/// typed. It is `Clone` (an event outlives the call that raised it), so the
/// one error that is not, [`BridgeError`], is held in an `Arc`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum RecoveryFailure {
    /// The replacement's load failed.
    ReloadFailed {
        /// The load's error.
        source: Arc<BridgeError>,
    },
    /// The replacement opened inside the host's process, which a served
    /// plugin's replacement must not.
    ReplacementInProcess,
    /// The replacement was dead before it was installed.
    ReplacementDied {
        /// Why.
        cause: PluginFailure,
    },
    /// The replacement's ports differ from the node's, which would leave the
    /// graph's wiring invalid.
    ReplacementPorts {
        /// The replacement's.
        got: PortCounts,
        /// The node's.
        was: PortCounts,
    },
    /// The graph refused the swap that installs the replacement.
    ReplacementRefused {
        /// The graph's refusal.
        source: CrossfadeRefused,
    },
    /// The reload's job was dropped by the session's spawner before it ran.
    ReloadDropped,
    /// The reload's job panicked.
    ReloadPanicked {
        /// The panic's message.
        message: String,
    },
    /// The reload did not finish within its allowance.
    ReloadTimedOut {
        /// The allowance.
        after: Duration,
    },
}

impl std::fmt::Display for RecoveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReloadFailed { source } => write!(f, "the replacement did not load: {source}"),
            Self::ReplacementInProcess => f.write_str("the replacement opened in process"),
            Self::ReplacementDied { cause } => {
                write!(f, "the replacement died before it was installed: {cause}")
            }
            Self::ReplacementPorts { got, was } => write!(
                f,
                "the replacement's ports ({got}) differ from the node's ({was})"
            ),
            Self::ReplacementRefused { source } => {
                write!(f, "the graph refused the replacement: {source}")
            }
            Self::ReloadDropped => f.write_str("the reload job was dropped"),
            Self::ReloadPanicked { message } => write!(f, "the reload job panicked: {message}"),
            Self::ReloadTimedOut { after } => {
                write!(f, "the reload did not finish in {after:?}")
            }
        }
    }
}

impl std::error::Error for RecoveryFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ReloadFailed { source } => Some(&**source),
            Self::ReplacementDied { cause } => Some(cause),
            Self::ReplacementRefused { source } => Some(source),
            Self::ReplacementInProcess
            | Self::ReplacementPorts { .. }
            | Self::ReloadDropped
            | Self::ReloadPanicked { .. }
            | Self::ReloadTimedOut { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    Running,
    /// Dead; a reload has been scheduled and not yet reported loaded.
    Reloading {
        started: Instant,
    },
    GaveUp,
}

/// One plugin's recovery state.
#[derive(Debug, Clone)]
pub struct Recovery {
    policy: RecoveryPolicy,
    failures: VecDeque<Instant>,
    phase: State,
}

impl Recovery {
    /// A running plugin.
    #[must_use]
    pub fn new(policy: RecoveryPolicy) -> Self {
        Self {
            policy,
            failures: VecDeque::new(),
            phase: State::Running,
        }
    }

    /// The policy it was made with.
    #[must_use]
    pub fn policy(&self) -> RecoveryPolicy {
        self.policy
    }

    /// Failures within the window now.
    #[must_use]
    pub fn failures(&self) -> u32 {
        u32::try_from(self.failures.len()).unwrap_or(u32::MAX)
    }

    /// Whether a replacement is being waited for.
    #[must_use]
    pub fn is_reloading(&self) -> bool {
        matches!(self.phase, State::Reloading { .. })
    }

    /// Whether recovery has stopped for good.
    #[must_use]
    pub fn has_given_up(&self) -> bool {
        self.phase == State::GaveUp
    }

    /// When the reload being waited for began, if one is.
    #[must_use]
    pub fn reload_started(&self) -> Option<Instant> {
        match self.phase {
            State::Reloading { started } => Some(started),
            _ => None,
        }
    }

    /// Takes one observation at `now` and says what to do.
    pub fn step(&mut self, event: RecoveryEvent, now: Instant) -> RecoveryAction {
        match (&mut self.phase, event) {
            (State::GaveUp, _) => RecoveryAction::Wait,
            (State::Running, RecoveryEvent::Died { cause }) => {
                self.fail(ReloadCause::Died(cause), now)
            }
            (State::Running, RecoveryEvent::LoadFailed { .. } | RecoveryEvent::Loaded) => {
                RecoveryAction::Wait
            }
            (State::Running, RecoveryEvent::Tick) => RecoveryAction::Wait,
            (State::Reloading { .. }, RecoveryEvent::Loaded) => {
                self.phase = State::Running;
                RecoveryAction::Wait
            }
            (State::Reloading { .. }, RecoveryEvent::LoadFailed { cause }) => {
                self.fail(ReloadCause::Attempt(cause), now)
            }
            (State::Reloading { started }, RecoveryEvent::Tick) => {
                if now.saturating_duration_since(*started) >= self.policy.load_timeout {
                    let cause = RecoveryFailure::ReloadTimedOut {
                        after: self.policy.load_timeout,
                    };
                    self.fail(ReloadCause::Attempt(cause), now)
                } else {
                    RecoveryAction::Wait
                }
            }
            (State::Reloading { .. }, RecoveryEvent::Died { .. }) => RecoveryAction::Wait,
        }
    }

    /// Records a failure at `now`: another reload, or the end of trying.
    fn fail(&mut self, cause: ReloadCause, now: Instant) -> RecoveryAction {
        while self
            .failures
            .front()
            .is_some_and(|&t| now.saturating_duration_since(t) >= self.policy.window)
        {
            self.failures.pop_front();
        }
        self.failures.push_back(now);
        let failures = u32::try_from(self.failures.len()).unwrap_or(u32::MAX);
        if failures > self.policy.max_failures {
            self.phase = State::GaveUp;
            return RecoveryAction::GiveUp(GiveUp {
                failures,
                window: self.policy.window,
                last: cause,
            });
        }
        let doublings = (failures - 1).min(16);
        let delay = self
            .policy
            .backoff
            .saturating_mul(1 << doublings)
            .min(self.policy.backoff_cap);
        self.phase = State::Reloading {
            started: now + delay,
        };
        RecoveryAction::Reload {
            delay,
            attempt: failures,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Table and sequence tests over `now` as a value: no sleeps, no threads.

    use super::*;

    const S: Duration = Duration::from_secs(1);

    /// Whether two actions are the same decision: a failure's cause is not
    /// compared here (a test that cares matches it).
    fn action_eq(a: &RecoveryAction, b: &RecoveryAction) -> bool {
        match (a, b) {
            (RecoveryAction::Wait, RecoveryAction::Wait) => true,
            (
                RecoveryAction::Reload { delay, attempt },
                RecoveryAction::Reload {
                    delay: d,
                    attempt: n,
                },
            ) => delay == d && attempt == n,
            (RecoveryAction::GiveUp(a), RecoveryAction::GiveUp(b)) => {
                a.failures == b.failures && a.window == b.window
            }
            _ => false,
        }
    }

    macro_rules! assert_action {
        ($left:expr, $right:expr $(,)?) => {{
            let (left, right) = (&$left, &$right);
            assert!(action_eq(left, right), "{left:?} != {right:?}");
        }};
        ($left:expr, $right:expr, $($msg:tt)+) => {{
            let (left, right) = (&$left, &$right);
            assert!(action_eq(left, right), "{left:?} != {right:?}: {}", format!($($msg)+));
        }};
    }

    fn policy() -> RecoveryPolicy {
        RecoveryPolicy {
            max_failures: 3,
            window: 60 * S,
            backoff: Duration::from_millis(250),
            backoff_cap: 2 * S,
            state_probation: 10 * S,
            load_timeout: 30 * S,
        }
    }

    /// A death by a stream that closed.
    fn died() -> RecoveryEvent {
        RecoveryEvent::Died {
            cause: PluginFailure::StreamClosed,
        }
    }

    /// A reload that failed, told apart from another by the allowance it
    /// names, in milliseconds.
    fn failed(id: u64) -> RecoveryEvent {
        RecoveryEvent::LoadFailed {
            cause: reload_timed_out(id),
        }
    }

    fn reload_timed_out(id: u64) -> RecoveryFailure {
        RecoveryFailure::ReloadTimedOut {
            after: Duration::from_millis(id),
        }
    }

    fn reload(delay_ms: u64, attempt: u32) -> RecoveryAction {
        RecoveryAction::Reload {
            delay: Duration::from_millis(delay_ms),
            attempt,
        }
    }

    /// A death reloads after the base backoff, and a good load returns to
    /// running with the budget intact.
    #[test]
    fn a_death_reloads_and_a_load_returns_to_running() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        assert_action!(r.step(died(), t0), reload(250, 1));
        assert!(r.is_reloading());
        assert_action!(r.step(RecoveryEvent::Loaded, t0 + S), RecoveryAction::Wait);
        assert!(!r.is_reloading());
        // Dying again later is the second failure in the window.
        assert_action!(r.step(died(), t0 + 2 * S), reload(500, 2));
    }

    /// The wait doubles with each failure in the window, up to the cap.
    #[test]
    fn the_backoff_doubles_to_a_cap() {
        let t0 = Instant::now();
        let mut p = policy();
        p.max_failures = 10;
        let mut r = Recovery::new(p);
        let mut got = Vec::new();
        for i in 0..5 {
            let event = if i == 0 { died() } else { failed(0) };
            match r.step(event, t0 + i * S) {
                RecoveryAction::Reload { delay, .. } => got.push(delay.as_millis()),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(got, [250, 500, 1000, 2000, 2000]);
    }

    /// A plugin that crashes on load fails again and again, and is given up
    /// on at the failure past the budget, naming the last reason.
    #[test]
    fn a_plugin_that_fails_every_load_is_given_up_on() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        assert_action!(r.step(died(), t0), reload(250, 1));
        assert_action!(r.step(failed(1), t0 + S), reload(500, 2));
        assert_action!(r.step(failed(2), t0 + 2 * S), reload(1000, 3));
        let RecoveryAction::GiveUp(why) = r.step(failed(3), t0 + 3 * S) else {
            panic!("the fourth failure in the window gives up");
        };
        assert_eq!(why.failures, 4);
        assert!(
            matches!(why.last, ReloadCause::Attempt(RecoveryFailure::ReloadTimedOut { after })
                if after == Duration::from_millis(3)),
            "{:?}",
            why.last
        );
        assert!(r.has_given_up());
        assert_action!(r.step(died(), t0 + 4 * S), RecoveryAction::Wait);
        assert_action!(
            r.step(RecoveryEvent::Loaded, t0 + 5 * S),
            RecoveryAction::Wait
        );
    }

    /// Failures older than the window no longer count: a plugin that runs
    /// for longer than the window between crashes is never given up on.
    #[test]
    fn failures_leave_the_budget_with_the_window() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        for i in 0..20u32 {
            let now = t0 + 61 * S * i;
            assert_action!(r.step(died(), now), reload(250, 1), "death {i}");
            assert_action!(r.step(RecoveryEvent::Loaded, now + S), RecoveryAction::Wait);
        }
    }

    /// A reload that never reports back fails at the timeout.
    #[test]
    fn a_reload_that_never_finishes_times_out() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        r.step(died(), t0);
        let started = r.reload_started().expect("reloading");
        assert_eq!(started, t0 + Duration::from_millis(250));
        assert_action!(
            r.step(RecoveryEvent::Tick, started + 29 * S),
            RecoveryAction::Wait
        );
        assert_action!(
            r.step(RecoveryEvent::Tick, started + 30 * S),
            reload(500, 2)
        );
    }

    /// Whatever the order of events, the machine never reloads without a
    /// failure having been recorded, never reloads while one is under way,
    /// and never says anything once it has given up. A fixed pseudo-random
    /// walk over every event kind.
    #[test]
    fn no_sequence_breaks_the_machine() {
        let t0 = Instant::now();
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..200 {
            let mut r = Recovery::new(policy());
            let mut now = t0;
            for _ in 0..300 {
                now += Duration::from_millis(next() % 4_000);
                let event = match next() % 4 {
                    0 => died(),
                    1 => RecoveryEvent::Loaded,
                    2 => failed(6),
                    _ => RecoveryEvent::Tick,
                };
                let was_reloading = r.is_reloading();
                let was_over = r.has_given_up();
                let action = r.step(event.clone(), now);
                match action {
                    RecoveryAction::Reload { delay, attempt } => {
                        assert!(!was_over);
                        assert!(delay <= policy().backoff_cap);
                        assert!((1..=policy().max_failures).contains(&attempt));
                        assert!(r.is_reloading());
                        assert!(
                            !was_reloading
                                || matches!(
                                    event,
                                    RecoveryEvent::LoadFailed { .. } | RecoveryEvent::Tick
                                ),
                            "a reload under way is only retried on its failure"
                        );
                    }
                    RecoveryAction::GiveUp(_) => {
                        assert!(!was_over);
                        assert!(r.has_given_up());
                    }
                    RecoveryAction::Wait => {}
                }
                if was_over {
                    assert_action!(action, RecoveryAction::Wait);
                    assert!(r.has_given_up());
                }
            }
        }
    }
}
