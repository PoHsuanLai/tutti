//! Hang detection for a hosted plugin: a pure state machine
//! (`HangDetector::observe`) with time as a value and no thread, no I/O and
//! no plugin in it.
//!
//! A plugin that is alive but never returns from `process()` is declared hung
//! when, over [`HangPolicy::window`], blocks kept being abandoned (at least
//! [`HangPolicy::blocks`] of them) and no reply of any kind came. It is read
//! off two monotonic totals from the bridge, abandoned blocks and replies,
//! compared between observations: a reply, or no new abandoned block (a stopped
//! or restarted device sends none), starts the suspicion over. So a slow plugin
//! that still answers is never declared hung, and neither is a count that
//! merely stands still.
//!
//! The session runs one detector per hosted plugin whether or not a
//! [`RecoveryPolicy`](crate::RecoveryPolicy) is set: detection ends the hung
//! server, and what follows its death is recovery's.

use std::time::{Duration, Instant};

use tutti_plugin_types::TerminationReason;

/// When a plugin's unanswered blocks are a hang. Times are measured on the
/// `now` each observation is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HangPolicy {
    /// Consecutive abandoned blocks it takes to suspect a hang.
    pub blocks: u32,
    /// How long the suspicion must hold, continuously, to declare the hang.
    pub window: Duration,
    /// How long without a newly abandoned block means blocks stopped coming
    /// (a stopped or restarted device), not that the plugin is slow to
    /// abandon: the suspicion starts over. It must exceed the longest
    /// interval between abandoned blocks, which is the block period (the
    /// bridge abandons one per block). A block period above it (about 1 s,
    /// 65536 frames at 48 kHz) goes undetected: every gap would read as a
    /// stopped device.
    pub stall_gap: Duration,
}

impl Default for HangPolicy {
    /// A hang after 5 s of 16 or more blocks lost in a row.
    ///
    /// The window is long because a plugin may legitimately block `process()`
    /// for seconds (a sample library loading on its first note); the block
    /// count only keeps a stopped device, which abandons nothing, from reading
    /// as a hang.
    fn default() -> Self {
        Self {
            blocks: 16,
            window: Duration::from_secs(5),
            stall_gap: Duration::from_secs(1),
        }
    }
}

/// One plugin's hang detection: what it has seen, read against the session's
/// [`HangPolicy`].
#[derive(Debug, Clone, Default)]
pub(crate) struct HangDetector {
    /// The previous observation's totals.
    last: Option<(u64, u64)>,
    /// When the current unbroken starvation began, with the abandoned total
    /// then.
    since: Option<(Instant, u64)>,
    /// When an abandoned block was last seen to be new.
    grew_at: Option<Instant>,
    /// Whether the hang was declared: it is declared once.
    declared: bool,
}

impl HangDetector {
    /// Whether the plugin is being watched go quiet: blocks keep being
    /// abandoned with no reply, and the hang is not yet declared.
    #[must_use]
    pub(crate) fn is_stalled(&self) -> bool {
        self.since.is_some() && !self.declared
    }

    /// Takes the bridge's totals at `now`: the reason to end the plugin's
    /// server, once, when the hang is declared.
    pub(crate) fn observe(
        &mut self,
        policy: HangPolicy,
        abandoned: u64,
        replies: u64,
        now: Instant,
    ) -> Option<TerminationReason> {
        let before = self.last.replace((abandoned, replies));
        if let Some((was_abandoned, was_replies)) = before {
            if abandoned > was_abandoned {
                self.grew_at = Some(now);
            }
            if replies != was_replies {
                // It answered: not a hang.
                self.since = None;
            } else if abandoned > was_abandoned {
                if self.since.is_none() {
                    self.since = Some((now, was_abandoned));
                }
            } else if self
                .grew_at
                .is_none_or(|g| now.saturating_duration_since(g) >= policy.stall_gap)
            {
                // Nothing new to abandon for a while: the blocks stopped,
                // which says nothing about the plugin.
                self.since = None;
            }
        }
        let (at, base) = self.since?;
        let blocks = abandoned.saturating_sub(base);
        let held = now.saturating_duration_since(at);
        if blocks >= u64::from(policy.blocks) && held >= policy.window && !self.declared {
            self.declared = true;
            return Some(TerminationReason::Hung { blocks, held });
        }
        None
    }
}

#[cfg(test)]
mod tests {
    //! Table and sequence tests over `now` as a value: no sleeps, no threads.

    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn policy() -> HangPolicy {
        HangPolicy {
            blocks: 16,
            window: 5 * S,
            stall_gap: S,
        }
    }

    fn hung(d: &mut HangDetector, at: Instant, abandoned: u64, replies: u64) -> bool {
        d.observe(policy(), abandoned, replies, at).is_some()
    }

    /// A hang is declared only when blocks keep being abandoned, with no
    /// reply, for the window; it ends the server once, however long it is
    /// then watched.
    #[test]
    fn a_hang_needs_blocks_and_time() {
        let t0 = Instant::now();
        let mut d = HangDetector::default();
        assert!(!hung(&mut d, t0, 100, 5), "baseline");
        assert!(!hung(&mut d, t0 + S, 200, 5), "starts");
        assert!(!hung(&mut d, t0 + 4 * S, 300, 5), "3 s");
        assert!(hung(&mut d, t0 + 6 * S, 400, 5), "5 s");
        assert!(!hung(&mut d, t0 + 7 * S, 500, 5), "once");
    }

    /// **A plugin is stalled while its blocks go unanswered, before the hang is
    /// declared**: not at the baseline, true once blocks are abandoned with no
    /// reply, false when it answers, and false again once the hang is
    /// declared.
    #[test]
    fn a_plugin_is_stalled_between_the_first_abandoned_block_and_the_verdict() {
        let t0 = Instant::now();
        let mut d = HangDetector::default();
        let _ = hung(&mut d, t0, 100, 5);
        assert!(!d.is_stalled(), "the baseline watches nothing");
        let _ = hung(&mut d, t0 + S, 200, 5);
        assert!(d.is_stalled(), "blocks abandoned, no reply");
        let _ = hung(&mut d, t0 + 2 * S, 210, 6);
        assert!(!d.is_stalled(), "a reply ends the suspicion");
        let _ = hung(&mut d, t0 + 3 * S, 300, 6);
        assert!(d.is_stalled());
        assert!(hung(&mut d, t0 + 9 * S, 500, 6));
        assert!(!d.is_stalled(), "the hang is declared: its server is ended");
    }

    /// Few blocks over a long time is not a hang, and neither is a long run
    /// of blocks over a short time.
    #[test]
    fn neither_time_nor_blocks_alone_is_a_hang() {
        let t0 = Instant::now();
        let mut d = HangDetector::default();
        for i in 0..20u64 {
            assert!(!hung(&mut d, t0 + i as u32 * S, i.min(15), 5));
        }
        let mut d = HangDetector::default();
        assert!(!hung(&mut d, t0, 0, 5));
        assert!(!hung(&mut d, t0 + S, 5_000, 5));
        assert!(!hung(&mut d, t0 + 2 * S, 9_000, 5));
    }

    /// **A count that stands still is not a hang.** The device stopped or
    /// restarted with blocks already abandoned and no reply owed yet: the
    /// totals freeze, nothing new is abandoned, and a live plugin is left
    /// alone however long that lasts. It resumes as a suspicion only from
    /// the blocks abandoned after it.
    #[test]
    fn a_count_that_stands_still_is_not_a_hang() {
        let t0 = Instant::now();
        let mut d = HangDetector::default();
        assert!(!hung(&mut d, t0, 10, 5));
        assert!(!hung(&mut d, t0 + S, 40, 5), "growing");
        for i in 2..60 {
            assert!(!hung(&mut d, t0 + i * S, 40, 5), "frozen at {i} s");
        }
        // The device comes back and the plugin does answer.
        assert!(!hung(&mut d, t0 + 61 * S, 41, 6));
    }

    /// **Observed faster than blocks are abandoned.** A frame every 16 ms
    /// and a block abandoned only every 48 ms (the bridge waits out each):
    /// most observations see no new block, and a hang is still declared
    /// after the window.
    #[test]
    fn a_hang_is_seen_by_an_observer_faster_than_the_blocks() {
        let t0 = Instant::now();
        let mut d = HangDetector::default();
        let mut told = None;
        for i in 0..1000u64 {
            let at = t0 + Duration::from_millis(16 * i);
            if hung(&mut d, at, i / 3, 5) {
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
    #[test]
    fn a_plugin_that_still_answers_is_not_hung() {
        let t0 = Instant::now();
        let mut d = HangDetector::default();
        // 20 blocks lost, a reply, again, for a minute.
        for i in 0..60u64 {
            assert!(!hung(&mut d, t0 + i as u32 * S, 20 * i, i), "i {i}");
        }
        // A reply between two readings 6 s apart: 20 then 30 abandoned.
        let mut d = HangDetector::default();
        assert!(!hung(&mut d, t0, 20, 1));
        assert!(!hung(&mut d, t0 + 6 * S, 30, 2), "answered");
        assert!(!hung(&mut d, t0 + 7 * S, 40, 2), "fresh");
        assert!(!hung(&mut d, t0 + 11 * S, 90, 2), "4 s");
        assert!(
            hung(&mut d, t0 + 12 * S, 100, 2),
            "5 s from the fresh start"
        );
    }
}
