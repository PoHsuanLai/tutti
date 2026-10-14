//! The cost model (doc 013, compiler step 6; Phase 6 part (b)).
//!
//! The compiler estimates every op's work per block from what the node
//! declared ([`Shape::cost`](crate::Shape::cost)) and its channel count, as
//! an affine function of the block length ([`Work`]). It uses the estimates
//! twice:
//!
//! - **Coarsening** (`compile/coarsen.rs`): sibling tasks (one
//!   predecessor set) cheaper than [`GRAIN`] are packed together until a
//!   task is worth dispatching, where that does not lengthen the critical
//!   path.
//! - **The plan's totals**: [`Plan::work`](crate::Plan::work), the whole
//!   block's work, and [`Plan::critical_path`](crate::Plan::critical_path),
//!   the costliest path through the task DAG, every hand-over from a task
//!   to a successor charged [`TASK_DISPATCH`] per place in the queue.
//!
//! [`schedule`] turns those totals into a [`Schedule`] for a pool of `p`
//! participants: serial, or parallel from some block length up, waking a
//! given number of participants. It is pure arithmetic on the plan, so a
//! test can check it from `compile` alone; the executor evaluates it when a
//! plan meets a pool (`Executor::set_pool`, or the rebuild that applies a
//! plan), never per block.
//!
//! # Why declared, not measured
//!
//! The estimate is built from a coarse declared class per node times its
//! channels times the block length. Two alternatives were weighed:
//!
//! - **Channels × frames alone** cannot tell a gain from a filter from a
//!   plugin: they differ by two to three orders of magnitude at the same
//!   width, and the one decision that matters (is a sibling worth a task of
//!   its own?) turns on exactly that difference.
//! - **Measuring** each task on the audio thread would know real costs, but
//!   needs a clock read around every task (tens of nanoseconds, the size of
//!   the tiny tasks it is meant to judge), makes the plan's shape depend on
//!   timing noise (so a test cannot pin it), and would have to restructure
//!   tasks on the audio thread or round-trip to the compiler to coarsen.
//!
//! A declared class is pure and deterministic, costs nothing per block, and
//! errs safely: the default, [`Cost::Heavy`], is the cost of a node that
//! declares nothing (a plugin, a host's node), so any two independent such
//! nodes are each worth a task and spread at any block length. For a graph
//! that declares nothing, three things still differ from part (a): its
//! copy and event ops are priced cheap, so sibling ones pack (where that
//! does not lengthen the critical path); the wake count is the larger of
//! the average parallelism and the widest level, at least 2; and a plan
//! whose only parallelism is in those cheap ops can run serially (24 of
//! the differential generator's 510 undeclared multi-task plans do, at
//! four participants). Only a node that says it is cheap is packed itself.
//! A wrong `Light` costs at most the parallel speed-up of a graph its
//! author said was trivial.
//!
//! # The constants
//!
//! Work is counted in **units of about a nanosecond of one core** on the
//! machine the constants were calibrated on (doc 013, "Phase 6, part (b)":
//! a 4-core container, `tutti-nodes`' `parallel_render` bench). Only their
//! ratios matter to a decision: a faster core shrinks node work and
//! dispatch alike. They are a speed decision only — the parallel render is
//! bit for bit the serial one, so a wrong estimate never changes a sample.

use crate::node::Shape;
use crate::plan::Op;
use tutti_types::Samples;

/// What a node declares about its processing cost, per sample and channel:
/// an order of magnitude the compiler's cost model works from (see
/// [`Shape::cost`](crate::Shape::cost)).
///
/// A speed hint only. Whatever a node declares, the parallel render is bit
/// for bit the serial one; a wrong class can only make a block slower.
///
/// `#[non_exhaustive]`: a finer class may come later without a breaking
/// change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Cost {
    /// A few arithmetic operations per sample and channel: a gain, a pan, a
    /// sum, a meter. Siblings this cheap are packed into one task, and a
    /// graph of nothing else renders serially.
    Light,
    /// Tens of operations per sample and channel: a biquad or
    /// state-variable filter, an oscillator, a dynamics detector.
    Moderate,
    /// Expensive, or unknown: a hosted plugin (whose call crosses a process
    /// boundary), a convolution, a synth. **The default**, so a node that
    /// declares nothing is assumed worth a task of its own at any block
    /// length.
    #[default]
    Heavy,
}

impl Cost {
    /// `(fixed, per sample per channel)` work, in the model's units.
    const fn weights(self) -> (u64, u64) {
        match self {
            // A gain's multiply plus its loop; the call itself.
            Cost::Light => (100, 1),
            // `SvfFilterNode<f64>`: about 8 ns per frame on the bench box.
            Cost::Moderate => (200, 8),
            // A plugin: an out-of-process call is tens of microseconds
            // before any DSP. Large enough that two independent ones always
            // spread, at any block length.
            Cost::Heavy => (20_000, 256),
        }
    }
}

/// An estimate of work per block, affine in the block length:
/// `fixed + per_frame × frames`, in the cost model's units (about a
/// nanosecond of one core; see the module docs of `cost.rs` in doc 013,
/// "Phase 6, part (b)").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Work {
    /// Work per block whatever its length.
    pub fixed: u64,
    /// Work per frame.
    pub per_frame: u64,
}

impl Work {
    /// No work.
    pub const ZERO: Self = Self {
        fixed: 0,
        per_frame: 0,
    };

    /// The estimate for a block of `frames`.
    pub const fn at(self, frames: Samples) -> u64 {
        self.fixed
            .saturating_add(self.per_frame.saturating_mul(frames.0 as u64))
    }

    /// The sum of two estimates.
    #[must_use]
    pub const fn plus(self, other: Self) -> Self {
        Self {
            fixed: self.fixed.saturating_add(other.fixed),
            per_frame: self.per_frame.saturating_add(other.per_frame),
        }
    }
}

/// How a plan's blocks run on a pool: what [`Plan::schedule`](crate::Plan::schedule)
/// decides from the cost model.
///
/// `#[non_exhaustive]`: a later schedule (per-level wake counts, say) is not
/// a breaking change; a `match` outside this crate needs a wildcard arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Schedule {
    /// Every block runs the serial walk on the calling thread: spreading
    /// would not pay for its dispatch (or there is one task, or one
    /// participant).
    Serial,
    /// Blocks of at least `from` frames spread over `wake` participants
    /// (the caller included); shorter blocks run the serial walk.
    Parallel {
        /// Participants a block wakes, the caller included: at least 2, at
        /// most the pool's: the plan's average parallelism (work over
        /// critical path) or its widest level, whichever is more.
        wake: usize,
        /// The shortest block that spreads.
        from: Samples,
    },
}

/// Charged to every task on top of its ops: taking it off the ready list,
/// its activation counters, its claims, and the cache lines its inputs
/// cross between cores on.
pub(crate) const TASK_DISPATCH: u64 = 400;

/// Charged once to a parallel block: arming the task graph, seeding the
/// shared flags, waking helpers and waiting for the last to leave.
pub(crate) const BLOCK_DISPATCH: u64 = 6_000;

/// A task estimated below this at the plan's maximum block is packed with
/// its independent siblings (`compile/coarsen.rs`): five dispatches' worth.
pub(crate) const GRAIN: u64 = 5 * TASK_DISPATCH;

/// A copy of one channel (a global input, an output, a PDC delay, a
/// feedback capture).
const COPY: Work = Work {
    fixed: 50,
    per_frame: 1,
};

/// An event op (a merge, an event delay, an event capture).
const EVENTS: Work = Work {
    fixed: 100,
    per_frame: 0,
};

/// The estimated work of `op`, whose node ops run units of `shapes` (by
/// plan unit index).
pub(crate) fn op_work(op: &Op, shapes: &[Shape]) -> Work {
    match *op {
        Op::GlobalIn { .. } | Op::Delay { .. } | Op::Output { .. } | Op::Capture { .. } => COPY,
        Op::EventDelay { .. } | Op::EventMerge { .. } | Op::EventCapture { .. } => EVENTS,
        Op::Node { unit, .. } => node_work(&shapes[unit as usize]),
    }
}

/// A node's estimated work: its declared class, per channel of its wider
/// side (at least one: an event-only node still runs per frame).
pub(crate) fn node_work(shape: &Shape) -> Work {
    let (fixed, per) = shape.cost.weights();
    let channels = shape.audio_in.count().max(shape.audio_out.count()).max(1) as u64;
    Work {
        fixed,
        per_frame: per * channels,
    }
}

/// The schedule for a plan of `tasks` tasks, `width` on its widest level,
/// estimated `work` per block and `critical` along its costliest task chain
/// (dispatch included), on a pool of `participants`, for blocks up to
/// `max_block`.
///
/// A block spreads when the parallel estimate beats the serial one:
///
/// ```text
/// BLOCK_DISPATCH + max(critical, (work + tasks × TASK_DISPATCH) / p)  <  work
/// ```
///
/// with `p` the participants it would wake. Both sides are affine in the
/// block length and the left grows no faster than the right, so if it pays
/// at all, it pays from some length `from` up; that length is found by
/// bisection. Integer arithmetic only, so the decision is exact and the
/// same everywhere.
pub(crate) fn schedule(
    work: Work,
    critical: Work,
    tasks: usize,
    width: usize,
    max_block: Samples,
    participants: usize,
) -> Schedule {
    if participants < 2 || tasks < 2 || max_block.0 == 0 {
        return Schedule::Serial;
    }
    let w_max = work.at(max_block);
    let c_max = critical.at(max_block).max(1);
    // The average parallelism (rounded up) or the widest level, whichever
    // is more: the first is what a block can keep busy on average, the
    // second how many tasks one level can have ready at once.
    let wake = w_max
        .div_ceil(c_max)
        .max(width as u64)
        .clamp(2, participants as u64) as usize;
    let p = wake as u64;
    let dispatch = TASK_DISPATCH.saturating_mul(tasks as u64);
    // `p × (parallel estimate) < p × serial`, without a division.
    let pays = |frames: usize| {
        let f = Samples(frames);
        let (w, c) = (work.at(f), critical.at(f));
        let par = p
            .saturating_mul(BLOCK_DISPATCH)
            .saturating_add(p.saturating_mul(c).max(w.saturating_add(dispatch)));
        par < p.saturating_mul(w)
    };
    if !pays(max_block.0) {
        return Schedule::Serial;
    }
    let (mut lo, mut hi) = (1usize, max_block.0);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if pays(mid) {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    Schedule::Parallel {
        wake,
        from: Samples(lo),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn w(fixed: u64, per_frame: u64) -> Work {
        Work { fixed, per_frame }
    }

    #[test]
    fn one_task_or_one_participant_is_serial() {
        let big = w(1_000_000, 1_000);
        assert_eq!(
            schedule(big, w(1, 0), 1, 1, Samples(256), 8),
            Schedule::Serial
        );
        assert_eq!(
            schedule(big, w(1, 0), 64, 64, Samples(256), 1),
            Schedule::Serial
        );
    }

    /// Mutations: `pays` always true (always parallel) → the tiny case
    /// fails; always false (always serial) → the wide case fails.
    #[test]
    fn spreads_wide_work_and_keeps_tiny_work_serial() {
        // Eight equal lanes of about 20 µs at 256 frames; it pays from 13
        // frames up (`24 × 79 × f > 27_600 - 24 × 200`).
        let lane = w(200, 79);
        let work = w(8 * lane.fixed, 8 * lane.per_frame);
        let critical = lane.plus(w(TASK_DISPATCH, 0));
        match schedule(work, critical, 9, 8, Samples(256), 4) {
            Schedule::Parallel { wake, from } => {
                assert_eq!(wake, 4);
                assert_eq!(from, Samples(13));
                // `from` is the threshold: it pays there and not one below.
                let at = |f| schedule(work, critical, 9, 8, Samples(f), 4);
                assert!(matches!(at(from.0), Schedule::Parallel { .. }));
                assert_eq!(at(from.0 - 1), Schedule::Serial);
            }
            s => panic!("{s:?}"),
        }
        // Sixteen gains' worth: about 10 µs of work in all.
        assert_eq!(
            schedule(w(2_000, 32), w(1_000, 20), 4, 2, Samples(256), 4),
            Schedule::Serial
        );
    }

    #[test]
    fn wakes_the_wider_of_parallelism_and_level_width() {
        // Work twice the critical path, two tasks a level: two
        // participants, whatever the pool.
        let work = w(200_000, 0);
        let critical = w(100_000, 0);
        assert_eq!(
            schedule(work, critical, 4, 2, Samples(64), 8),
            Schedule::Parallel {
                wake: 2,
                from: Samples(1)
            }
        );
        // Three tasks on one level: three.
        assert_eq!(
            schedule(work, critical, 4, 3, Samples(64), 8),
            Schedule::Parallel {
                wake: 3,
                from: Samples(1)
            }
        );
    }

    #[test]
    fn a_default_node_is_worth_a_task_at_one_frame() {
        let heavy = node_work(&Shape::audio(
            tutti_types::ChannelLayout::MONO,
            tutti_types::ChannelLayout::MONO,
        ));
        let task = heavy.plus(w(TASK_DISPATCH, 0));
        assert_eq!(
            schedule(heavy.plus(heavy), task, 2, 2, Samples(1), 2),
            Schedule::Parallel {
                wake: 2,
                from: Samples(1)
            }
        );
    }
}
