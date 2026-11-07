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
//! # Two budgets
//!
//! - **Give up.** At most [`RecoveryPolicy::max_failures`] failures (a death,
//!   or a reload that did not come up) within [`RecoveryPolicy::window`]. A
//!   plugin that crashes on load fails again and again inside the window and
//!   is given up on; one that runs for longer than the window between
//!   crashes never is.
//! - **Hang.** A plugin that is alive but never returns from `process()` is
//!   declared hung when, over [`RecoveryPolicy::hang_window`], blocks kept
//!   being abandoned (at least [`RecoveryPolicy::hang_blocks`] of them) and
//!   no reply of any kind came. It is read off two monotonic totals from the
//!   bridge, abandoned blocks and replies, compared between observations: a
//!   reply, or no new abandoned block (a stopped or restarted device sends
//!   none), starts the suspicion over. So a slow plugin that still answers
//!   is never declared hung, and neither is a count that merely stands
//!   still.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

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
    /// Consecutive abandoned blocks it takes to suspect a hang.
    pub hang_blocks: u32,
    /// How long the suspicion must hold, continuously, to declare the hang.
    pub hang_window: Duration,
    /// How long without a newly abandoned block means blocks stopped coming
    /// (a stopped or restarted device), not that the plugin is slow to
    /// abandon: the suspicion starts over. It must exceed the longest
    /// interval between abandoned blocks, which is the block period (the
    /// bridge abandons one per block). A block period above it (about 1 s,
    /// 65536 frames at 48 kHz) goes undetected: every gap would read as a
    /// stopped device.
    pub stall_gap: Duration,
    /// How long after a plugin restores a snapshot a death is taken to be
    /// the snapshot's doing: the next load goes without it
    /// ([`StateRestore::Withheld`](crate::StateRestore::Withheld)).
    pub state_probation: Duration,
    /// How long a reload may run before it counts as failed.
    pub load_timeout: Duration,
}

impl Default for RecoveryPolicy {
    /// Three failures in a minute, waits of 0.25 s doubling to 8 s, a hang
    /// after 5 s of 16 or more blocks lost in a row, and a minute to load.
    ///
    /// The hang window is long because a plugin may legitimately block
    /// `process()` for seconds (a sample library loading on its first note);
    /// the block count only keeps a stopped device, which abandons nothing,
    /// from reading as a hang.
    fn default() -> Self {
        Self {
            max_failures: 3,
            window: Duration::from_secs(60),
            backoff: Duration::from_millis(250),
            backoff_cap: Duration::from_secs(8),
            hang_blocks: 16,
            hang_window: Duration::from_secs(5),
            stall_gap: Duration::from_secs(1),
            state_probation: Duration::from_secs(10),
            load_timeout: Duration::from_secs(60),
        }
    }
}

/// What the session observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryEvent {
    /// The plugin was declared dead, with the engine's latched cause.
    Died {
        /// Why.
        cause: String,
    },
    /// The reload came up and is in the graph.
    Loaded,
    /// The reload did not come up.
    LoadFailed {
        /// Why.
        reason: String,
    },
    /// The running plugin's totals from the bridge, since it started.
    Starved {
        /// Blocks the bridge gave up waiting for.
        abandoned: u64,
        /// Replies it took, on time or owed.
        replies: u64,
    },
    /// Time passed with nothing else to report.
    Tick,
}

/// What the session does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryAction {
    /// Nothing.
    Wait,
    /// End the plugin's server: it is hung. Its death follows as
    /// [`RecoveryEvent::Died`].
    Terminate {
        /// Why, as the death will read.
        cause: String,
    },
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GiveUp {
    /// Failures within the window, including this one.
    pub failures: u32,
    /// The window they fell in.
    pub window: Duration,
    /// The last failure's reason.
    pub last: String,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Running. `last` is the previous observation's totals; `since` when
    /// the current unbroken starvation began, with the abandoned total then.
    Running {
        last: Option<(u64, u64)>,
        since: Option<(Instant, u64)>,
        /// When an abandoned block was last seen to be new.
        grew_at: Option<Instant>,
        terminating: bool,
    },
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
            phase: State::Running {
                last: None,
                since: None,
                grew_at: None,
                terminating: false,
            },
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
            (State::Running { .. }, RecoveryEvent::Died { cause }) => self.fail(cause, now),
            (State::Running { .. }, RecoveryEvent::LoadFailed { .. } | RecoveryEvent::Loaded) => {
                RecoveryAction::Wait
            }
            (State::Running { .. }, RecoveryEvent::Tick) => RecoveryAction::Wait,
            (
                State::Running {
                    last,
                    since,
                    grew_at,
                    terminating,
                },
                RecoveryEvent::Starved { abandoned, replies },
            ) => {
                let before = last.replace((abandoned, replies));
                if let Some((was_abandoned, was_replies)) = before {
                    if abandoned > was_abandoned {
                        *grew_at = Some(now);
                    }
                    if replies != was_replies {
                        // It answered: not a hang.
                        *since = None;
                    } else if abandoned > was_abandoned {
                        if since.is_none() {
                            *since = Some((now, was_abandoned));
                        }
                    } else if grew_at
                        .is_none_or(|g| now.saturating_duration_since(g) >= self.policy.stall_gap)
                    {
                        // Nothing new to abandon for a while: the blocks
                        // stopped, which says nothing about the plugin.
                        *since = None;
                    }
                }
                let verdict = since.map(|(at, base)| {
                    (
                        abandoned.saturating_sub(base),
                        now.saturating_duration_since(at),
                    )
                });
                match verdict {
                    Some((blocks, held))
                        if blocks >= u64::from(self.policy.hang_blocks)
                            && held >= self.policy.hang_window
                            && !*terminating =>
                    {
                        *terminating = true;
                        RecoveryAction::Terminate {
                            cause: format!(
                                "the plugin stopped answering ({blocks} blocks abandoned \
                                 over {held:?}, no reply)"
                            ),
                        }
                    }
                    _ => RecoveryAction::Wait,
                }
            }
            (State::Reloading { .. }, RecoveryEvent::Loaded) => {
                self.phase = State::Running {
                    last: None,
                    since: None,
                    grew_at: None,
                    terminating: false,
                };
                RecoveryAction::Wait
            }
            (State::Reloading { .. }, RecoveryEvent::LoadFailed { reason }) => {
                self.fail(reason, now)
            }
            (State::Reloading { started }, RecoveryEvent::Tick) => {
                if now.saturating_duration_since(*started) >= self.policy.load_timeout {
                    let reason = format!(
                        "the reload did not finish in {:?}",
                        self.policy.load_timeout
                    );
                    self.fail(reason, now)
                } else {
                    RecoveryAction::Wait
                }
            }
            (
                State::Reloading { .. },
                RecoveryEvent::Died { .. } | RecoveryEvent::Starved { .. },
            ) => RecoveryAction::Wait,
        }
    }

    /// Records a failure at `now`: another reload, or the end of trying.
    fn fail(&mut self, reason: String, now: Instant) -> RecoveryAction {
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
                last: reason,
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

    fn policy() -> RecoveryPolicy {
        RecoveryPolicy {
            max_failures: 3,
            window: 60 * S,
            backoff: Duration::from_millis(250),
            backoff_cap: 2 * S,
            hang_blocks: 16,
            hang_window: 5 * S,
            stall_gap: S,
            state_probation: 10 * S,
            load_timeout: 30 * S,
        }
    }

    fn died(cause: &str) -> RecoveryEvent {
        RecoveryEvent::Died {
            cause: cause.into(),
        }
    }

    fn failed(reason: &str) -> RecoveryEvent {
        RecoveryEvent::LoadFailed {
            reason: reason.into(),
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
        assert_eq!(r.step(died("segv"), t0), reload(250, 1));
        assert!(r.is_reloading());
        assert_eq!(r.step(RecoveryEvent::Loaded, t0 + S), RecoveryAction::Wait);
        assert!(!r.is_reloading());
        // Dying again later is the second failure in the window.
        assert_eq!(r.step(died("segv"), t0 + 2 * S), reload(500, 2));
    }

    /// The wait doubles with each failure in the window, up to the cap.
    ///
    /// Mutation (run, failed): no doubling (a constant delay).
    #[test]
    fn the_backoff_doubles_to_a_cap() {
        let t0 = Instant::now();
        let mut p = policy();
        p.max_failures = 10;
        let mut r = Recovery::new(p);
        let mut got = Vec::new();
        for i in 0..5 {
            let event = if i == 0 { died("no") } else { failed("no") };
            match r.step(event, t0 + i * S) {
                RecoveryAction::Reload { delay, .. } => got.push(delay.as_millis()),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(got, [250, 500, 1000, 2000, 2000]);
    }

    /// A plugin that crashes on load fails again and again, and is given up
    /// on at the failure past the budget, naming the last reason.
    ///
    /// Mutation (run, failed): the budget compared with `>=` instead of `>`
    /// (given up one failure early).
    #[test]
    fn a_plugin_that_fails_every_load_is_given_up_on() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        assert_eq!(r.step(died("segv"), t0), reload(250, 1));
        assert_eq!(r.step(failed("a"), t0 + S), reload(500, 2));
        assert_eq!(r.step(failed("b"), t0 + 2 * S), reload(1000, 3));
        let RecoveryAction::GiveUp(why) = r.step(failed("c"), t0 + 3 * S) else {
            panic!("the fourth failure in the window gives up");
        };
        assert_eq!(why.failures, 4);
        assert_eq!(why.last, "c");
        assert!(r.has_given_up());
        assert_eq!(r.step(died("late"), t0 + 4 * S), RecoveryAction::Wait);
        assert_eq!(
            r.step(RecoveryEvent::Loaded, t0 + 5 * S),
            RecoveryAction::Wait
        );
    }

    /// Failures older than the window no longer count: a plugin that runs
    /// for longer than the window between crashes is never given up on.
    ///
    /// Mutation (run, failed): pruning skipped (the fourth death gives up).
    #[test]
    fn failures_leave_the_budget_with_the_window() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        for i in 0..20u32 {
            let now = t0 + 61 * S * i;
            assert_eq!(r.step(died("segv"), now), reload(250, 1), "death {i}");
            assert_eq!(r.step(RecoveryEvent::Loaded, now + S), RecoveryAction::Wait);
        }
    }

    /// A reload that never reports back fails at the timeout.
    #[test]
    fn a_reload_that_never_finishes_times_out() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        r.step(died("segv"), t0);
        let started = r.reload_started().expect("reloading");
        assert_eq!(started, t0 + Duration::from_millis(250));
        assert_eq!(
            r.step(RecoveryEvent::Tick, started + 29 * S),
            RecoveryAction::Wait
        );
        assert_eq!(
            r.step(RecoveryEvent::Tick, started + 30 * S),
            reload(500, 2)
        );
    }

    fn starved(r: &mut Recovery, at: Instant, abandoned: u64, replies: u64) -> RecoveryAction {
        r.step(RecoveryEvent::Starved { abandoned, replies }, at)
    }

    fn terminates(a: RecoveryAction) -> bool {
        matches!(a, RecoveryAction::Terminate { .. })
    }

    /// A hang is declared only when blocks keep being abandoned, with no
    /// reply, for the window; it ends the server once, however long it is
    /// then watched.
    ///
    /// Mutations (each run, each failed): the window not checked (declared
    /// at the first observation over the block count); `terminating` not
    /// latched (a second `Terminate`).
    #[test]
    fn a_hang_needs_blocks_and_time() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        assert_eq!(
            starved(&mut r, t0, 100, 5),
            RecoveryAction::Wait,
            "baseline"
        );
        assert_eq!(
            starved(&mut r, t0 + S, 200, 5),
            RecoveryAction::Wait,
            "starts"
        );
        assert_eq!(
            starved(&mut r, t0 + 4 * S, 300, 5),
            RecoveryAction::Wait,
            "3 s"
        );
        assert!(terminates(starved(&mut r, t0 + 6 * S, 400, 5)), "5 s");
        assert_eq!(
            starved(&mut r, t0 + 7 * S, 500, 5),
            RecoveryAction::Wait,
            "once"
        );
        // Its death then follows the ordinary path.
        assert_eq!(r.step(died("terminated"), t0 + 8 * S), reload(250, 1));
    }

    /// Few blocks over a long time is not a hang, and neither is a long run
    /// of blocks over a short time.
    ///
    /// Mutation (run, failed): the block count not checked.
    #[test]
    fn neither_time_nor_blocks_alone_is_a_hang() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        for i in 0..20u64 {
            let at = t0 + i as u32 * S;
            assert_eq!(starved(&mut r, at, i.min(15), 5), RecoveryAction::Wait);
        }
        let mut r = Recovery::new(policy());
        assert_eq!(starved(&mut r, t0, 0, 5), RecoveryAction::Wait);
        assert_eq!(starved(&mut r, t0 + S, 5_000, 5), RecoveryAction::Wait);
        assert_eq!(starved(&mut r, t0 + 2 * S, 9_000, 5), RecoveryAction::Wait);
    }

    /// **A count that stands still is not a hang.** The device stopped or
    /// restarted with blocks already abandoned and no reply owed yet: the
    /// totals freeze, nothing new is abandoned, and a live plugin is left
    /// alone however long that lasts. It resumes as a suspicion only from
    /// the blocks abandoned after it.
    ///
    /// Mutation (run, failed): the stall not clearing the suspicion (the
    /// frozen totals are held for the window and ended).
    #[test]
    fn a_count_that_stands_still_is_not_a_hang() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        assert_eq!(starved(&mut r, t0, 10, 5), RecoveryAction::Wait);
        assert_eq!(
            starved(&mut r, t0 + S, 40, 5),
            RecoveryAction::Wait,
            "growing"
        );
        for i in 2..60 {
            assert_eq!(
                starved(&mut r, t0 + i * S, 40, 5),
                RecoveryAction::Wait,
                "frozen at {i} s"
            );
        }
        // The device comes back and the plugin does answer.
        assert_eq!(starved(&mut r, t0 + 61 * S, 41, 6), RecoveryAction::Wait);
    }

    /// **Observed faster than blocks are abandoned.** A frame every 16 ms
    /// and a block abandoned only every 48 ms (the bridge waits out each):
    /// most observations see no new block, and a hang is still declared
    /// after the window.
    ///
    /// Mutation (run, failed): `stall_gap` ignored (any observation without
    /// growth clears the suspicion).
    #[test]
    fn a_hang_is_seen_by_an_observer_faster_than_the_blocks() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        let mut told = None;
        for i in 0..1000u64 {
            let at = t0 + Duration::from_millis(16 * i);
            if terminates(starved(&mut r, at, i / 3, 5)) {
                told = Some(at - t0);
                break;
            }
        }
        let told = told.expect("a hang is declared");
        assert!(told >= 5 * S && told < 7 * S, "{told:?}");
    }

    /// A slow plugin that still answers is not declared hung: replies keep
    /// arriving between observations, even when the abandoned total keeps
    /// growing. A reply between two observations restarts the suspicion
    /// however far the count has grown since.
    ///
    /// Mutations (each run, each failed): a changed reply total not clearing
    /// the suspicion.
    #[test]
    fn a_plugin_that_still_answers_is_not_hung() {
        let t0 = Instant::now();
        let mut r = Recovery::new(policy());
        // 20 blocks lost, a reply, again, for a minute.
        for i in 0..60u64 {
            assert_eq!(
                starved(&mut r, t0 + i as u32 * S, 20 * i, i),
                RecoveryAction::Wait,
                "i {i}"
            );
        }
        // A reply between two readings 6 s apart: 20 then 30 abandoned.
        let mut r = Recovery::new(policy());
        assert_eq!(starved(&mut r, t0, 20, 1), RecoveryAction::Wait);
        assert_eq!(
            starved(&mut r, t0 + 6 * S, 30, 2),
            RecoveryAction::Wait,
            "answered"
        );
        assert_eq!(
            starved(&mut r, t0 + 7 * S, 40, 2),
            RecoveryAction::Wait,
            "fresh"
        );
        assert_eq!(
            starved(&mut r, t0 + 11 * S, 90, 2),
            RecoveryAction::Wait,
            "4 s"
        );
        assert!(
            terminates(starved(&mut r, t0 + 12 * S, 100, 2)),
            "5 s from the fresh start"
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
            let (mut abandoned, mut replies) = (0u64, 0u64);
            for _ in 0..300 {
                now += Duration::from_millis(next() % 4_000);
                let event = match next() % 5 {
                    0 => died("d"),
                    1 => RecoveryEvent::Loaded,
                    2 => failed("f"),
                    3 => {
                        abandoned += next() % 40;
                        replies += next() % 2;
                        RecoveryEvent::Starved { abandoned, replies }
                    }
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
                    RecoveryAction::Terminate { .. } => {
                        assert!(!was_reloading && !was_over);
                        assert!(matches!(event, RecoveryEvent::Starved { .. }));
                    }
                    RecoveryAction::Wait => {}
                }
                if was_over {
                    assert_eq!(action, RecoveryAction::Wait);
                    assert!(r.has_given_up());
                }
            }
        }
    }
}
