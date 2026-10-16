//! The plugin-death debounce: when a host stops believing a plugin is alive.
//!
//! The engine reports liveness as [`PluginStatus`](crate::handles::PluginStatus):
//! a latched death plus its cause. That answers *whether* a plugin died, but a
//! host acting on one observation of it races the engine:
//!
//! - **A failed call returns before the death is published.** The bridge marks
//!   the crash on its own thread with a `Release` store, so a host that checks
//!   immediately after a failed call can see the plugin alive, and the next
//!   check dead.
//! - **`Alive` does not mean healthy.** A peer that answers with a well-formed
//!   reply of the wrong kind leaves the engine reporting `Alive` while the call
//!   fails. How many failures over how long make a death is a host's policy;
//!   [`Liveness`] is that policy, written once.
//!
//! **Count and time, both.** A plugin is dead after at least
//! [`Liveness::DEATHS`] consecutive unhealthy observations spanning at least a
//! window ([`Liveness::DEFAULT_WINDOW`], 50 ms: three 60 Hz frames). The count
//! guards against one racy observation; the time keeps the window when a host
//! observes back to back, which would shrink a count alone to microseconds.
//! A healthy observation resets both.

use std::time::{Duration, Instant};

/// What a host believes about a plugin after an observation
/// ([`Liveness::observe`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Answering normally.
    Healthy,
    /// Unhealthy for `consecutive` observations in a row, but not yet for
    /// long enough to be written off.
    Failing {
        /// How many observations in a row found the plugin unhealthy
        /// (saturating at `u8::MAX`).
        consecutive: u8,
    },
    /// Written off: unhealthy for at least [`Liveness::DEATHS`] observations
    /// spanning at least the window. Latched: no later observation revives it,
    /// because the engine offers no relaunch.
    Dead,
}

/// The debounce between the engine's liveness report and a host's verdict:
/// dead after at least [`DEATHS`](Self::DEATHS) consecutive unhealthy
/// observations spanning at least a window, reset by a healthy one.
///
/// Pure: the host supplies each observation and the time it was made, so a
/// test passes its own `now` and needs no sleep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Liveness {
    window: Duration,
    /// The run of unhealthy observations: its length and when it began.
    failing: Option<(u8, Instant)>,
    dead: bool,
}

impl Liveness {
    /// The fewest consecutive unhealthy observations that make a death.
    ///
    /// Three rather than one because the crash flag is published from the
    /// bridge's thread after the failing call returns, so a single
    /// observation races it.
    pub const DEATHS: u8 = 3;

    /// The default window the unhealthy run must span: 50 ms, three 60 Hz
    /// frames, far above the crash flag's publish window and far below what
    /// a user notices.
    pub const DEFAULT_WINDOW: Duration = Duration::from_millis(50);

    /// A plugin believed healthy, debounced over [`DEFAULT_WINDOW`](Self::DEFAULT_WINDOW).
    pub const fn new() -> Self {
        Self::with_window(Self::DEFAULT_WINDOW)
    }

    /// A plugin believed healthy, debounced over `window`.
    pub const fn with_window(window: Duration) -> Self {
        Self {
            window,
            failing: None,
            dead: false,
        }
    }

    /// The window an unhealthy run must span.
    pub const fn window(&self) -> Duration {
        self.window
    }

    /// Records one observation, made at `now`, and returns the verdict.
    ///
    /// Dead on the observation that is both at least the
    /// [`DEATHS`](Self::DEATHS)th unhealthy one in a row and at least the
    /// window after the first of them. A healthy observation resets the
    /// count and the window. A `now` earlier than the run's start counts as
    /// no time passed.
    pub fn observe(&mut self, healthy: bool, now: Instant) -> Verdict {
        if self.dead {
            return Verdict::Dead;
        }
        if healthy {
            self.failing = None;
            return Verdict::Healthy;
        }
        let (consecutive, since) = match self.failing {
            Some((n, since)) => (n.saturating_add(1), since),
            None => (1, now),
        };
        if consecutive >= Self::DEATHS && now.saturating_duration_since(since) >= self.window {
            self.failing = None;
            self.dead = true;
            return Verdict::Dead;
        }
        self.failing = Some((consecutive, since));
        Verdict::Failing { consecutive }
    }

    /// The verdict of the last observation (`Healthy` before any).
    pub fn verdict(&self) -> Verdict {
        match (self.dead, self.failing) {
            (true, _) => Verdict::Dead,
            (false, Some((consecutive, _))) => Verdict::Failing { consecutive },
            (false, None) => Verdict::Healthy,
        }
    }
}

impl Default for Liveness {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// The third consecutive unhealthy observation is a death once the run
    /// spans the window: at 0, 25 and 50 ms, the third is `Dead`.
    ///
    /// Mutation: `DEATHS` 4 (the third reads `Failing { 3 }`, failed);
    /// comparing the span with `>` (50 ms is not past 50 ms, failed).
    #[test]
    fn the_third_unhealthy_observation_spanning_the_window_is_dead() {
        let t0 = Instant::now();
        let mut l = Liveness::new();
        assert_eq!(l.observe(false, t0), Verdict::Failing { consecutive: 1 });
        assert_eq!(
            l.observe(false, t0 + 25 * MS),
            Verdict::Failing { consecutive: 2 }
        );
        assert_eq!(l.observe(false, t0 + 50 * MS), Verdict::Dead);
        assert_eq!(l.verdict(), Verdict::Dead);
    }

    /// Three unhealthy observations a microsecond apart are not a death: the
    /// count is reached, the window is not. A fourth once the window has
    /// passed is.
    ///
    /// Mutation: dropping the window test from `observe` (the third reads
    /// `Dead`, failed).
    #[test]
    fn three_observations_a_microsecond_apart_are_not_dead() {
        let us = Duration::from_micros(1);
        let t0 = Instant::now();
        let mut l = Liveness::new();
        for n in 1..=3u8 {
            assert_eq!(
                l.observe(false, t0 + us * u32::from(n - 1)),
                Verdict::Failing { consecutive: n },
                "observation {n} is inside the window"
            );
        }
        assert_eq!(
            l.observe(false, t0 + Liveness::DEFAULT_WINDOW),
            Verdict::Dead
        );
    }

    /// Past the window, the count still holds: two unhealthy observations a
    /// second apart are not a death.
    ///
    /// Mutation: dropping the count test from `observe` (the second reads
    /// `Dead`, failed).
    #[test]
    fn two_observations_past_the_window_are_not_dead() {
        let t0 = Instant::now();
        let mut l = Liveness::new();
        l.observe(false, t0);
        assert_eq!(
            l.observe(false, t0 + Duration::from_secs(1)),
            Verdict::Failing { consecutive: 2 }
        );
    }

    /// A healthy observation resets both the count and the window: after it,
    /// a new run needs three observations again, and the window counts from
    /// the new run's first.
    ///
    /// Mutations (each run, each failed): keeping the count across a healthy
    /// observation (the run after it reads `Failing { 3 }` at its first);
    /// keeping the run's start across it (the new run's third, 50 ms after
    /// the old run began but 2 ms after its own, reads `Dead`).
    #[test]
    fn a_healthy_observation_resets_the_count_and_the_window() {
        let t0 = Instant::now();
        let mut l = Liveness::new();
        l.observe(false, t0);
        l.observe(false, t0 + MS);
        assert_eq!(l.observe(true, t0 + 2 * MS), Verdict::Healthy);
        assert_eq!(l.verdict(), Verdict::Healthy);

        assert_eq!(
            l.observe(false, t0 + 48 * MS),
            Verdict::Failing { consecutive: 1 }
        );
        assert_eq!(
            l.observe(false, t0 + 49 * MS),
            Verdict::Failing { consecutive: 2 }
        );
        assert_eq!(
            l.observe(false, t0 + 50 * MS),
            Verdict::Failing { consecutive: 3 },
            "the window counts from this run's first observation, 2 ms ago"
        );
        assert_eq!(l.observe(false, t0 + 98 * MS), Verdict::Dead);
    }

    /// A death is latched: a healthy observation after it does not revive the
    /// plugin.
    ///
    /// Mutation: letting a healthy observation clear `dead` (the last reads
    /// `Healthy`, failed).
    #[test]
    fn a_death_is_latched() {
        let t0 = Instant::now();
        let mut l = Liveness::with_window(Duration::ZERO);
        for _ in 0..Liveness::DEATHS {
            l.observe(false, t0);
        }
        assert_eq!(l.verdict(), Verdict::Dead);
        assert_eq!(l.observe(true, t0 + MS), Verdict::Dead);
    }

    /// A plugin observed healthy is never written off, however long.
    ///
    /// Mutation: `observe` ignoring `healthy` (the first reads `Failing`,
    /// failed).
    #[test]
    fn a_healthy_plugin_is_never_dead() {
        let t0 = Instant::now();
        let mut l = Liveness::new();
        for s in 0..10 {
            assert_eq!(
                l.observe(true, t0 + Duration::from_secs(s)),
                Verdict::Healthy
            );
        }
    }
}
