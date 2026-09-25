//! One butler cycle, synchronously.
//!
//! The butler's work is a loop over three pure steps — publish PDC preroll
//! changes, apply direction changes to each ring's mapping, refill the rings
//! (following each reader to where it plays). None of them is asynchronous:
//! they read a `DashMap`, decode from a file, and write into a ring. The only async in the butler is the *pacing* —
//! the timer the loop parks on when there is nothing urgent to do.
//!
//! This module is that work with the pacing removed. [`ButlerCycle::step`]
//! drains the command queue, runs the three steps, and reports what the pacing
//! layer should do next as a [`StepOutcome`]. The async loop in
//! [`loop_body`](super::loop_body) is then the pacing layer and nothing else.
//!
//! # Why the split earns its name
//!
//! A test that has to observe a butler-driven stream otherwise has only wall
//! clock to work with: send a command, sleep, look, sleep again. That is not a
//! stylistic complaint. The butler parks 1 ms when idle and 3 ms when every
//! ring is healthy, so a poller reading at 5–10 ms is racing a producer it
//! cannot see, and every such test carries a timeout that is really a guess
//! about the machine it runs on. Driving the same steps directly makes the
//! question "has the butler done the work" answerable by *counting* rather
//! than by waiting — `step` returns only after the work is done.
//!
//! It is the same seam the production loop uses, not a parallel test path:
//! `butler_loop_async` is written in terms of `ButlerCycle::step`, so a
//! divergence between what tests drive and what ships is not representable.

use super::command::ButlerCommand;
use super::config::BufferConfig;
use super::handlers::{handle_command, Handles, Local};
use super::io::refill::{refill_all, refill_all_parallel};
use super::loops::{apply_mapping, Mapping};
use super::preroll::apply_pdc_updates;

/// What the pacing layer should do after a [`ButlerCycle::step`].
///
/// The butler runs at maximum thread priority, so "keep going" and "there is
/// nothing to do" must be distinguishable — spinning either one costs a core.
/// The step itself has no opinion on *how long* to park, only on whether
/// parking is safe, which is why this carries no duration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepOutcome {
    /// No channel is streaming. Nothing to refill; park on the command queue.
    Idle,
    /// Every streaming ring is at or above its refill threshold. The work ran,
    /// and none of it was urgent — park on the command queue.
    Healthy,
    /// At least one ring is below its refill threshold **and this cycle moved
    /// frames into one**. Come straight back; there is more to do.
    Busy,
    /// A ring is below its refill threshold and this cycle could put nothing
    /// into it.
    ///
    /// The threshold is a fraction of ring *capacity*, and capacity is sized
    /// from the whole file — so a stream reading its last seconds can never
    /// reach it however many cycles run. Reporting that as `Busy` makes the
    /// pacing layer skip its park and re-run immediately, which spins a
    /// max-priority thread flat out for the whole tail of every clip.
    ///
    /// It is also what a step-driven caller needs: "the butler has done all it
    /// can" is a *fixed point*, and a caller that waited for `Healthy` instead
    /// would wait forever near the end of a file.
    Stalled,
    /// Shutdown was signalled. Stop.
    Shutdown,
}

/// The butler's per-cycle state and the step over it.
///
/// Owns [`Local`] (butler-thread-local: producers, scratch, the region-id
/// source) and borrows nothing. Both the async loop and a test driver build one
/// and call [`step`](Self::step) repeatedly; the only difference between them is
/// what happens *between* the calls.
pub(crate) struct ButlerCycle {
    local: Local,
    config: BufferConfig,
}

impl ButlerCycle {
    /// A cycle whose scratch is pre-sized for one `config.chunk_size` refill.
    pub(crate) fn new(config: BufferConfig) -> Self {
        Self {
            local: Local::new(config.chunk_size),
            config,
        }
    }

    /// Run one cycle: drain every queued command, then — when any channel is
    /// streaming — publish PDC preroll changes, apply direction changes, and
    /// refill the rings.
    ///
    /// Direction changes are applied *before* refill so the refill writes the
    /// new mapping this cycle.
    ///
    /// `drain` is called until it yields `None`; commands are applied
    /// non-blockingly, so a step never waits for one to arrive.
    pub(crate) fn step(
        &mut self,
        shared: &Handles,
        mut drain: impl FnMut() -> Option<ButlerCommand>,
    ) -> StepOutcome {
        while let Some(cmd) = drain() {
            if matches!(cmd, ButlerCommand::Shutdown) {
                handle_command(cmd, shared, &self.config, &mut self.local);
                return StepOutcome::Shutdown;
            }
            handle_command(cmd, shared, &self.config, &mut self.local);
        }

        if shared.plans.is_empty() {
            return StepOutcome::Idle;
        }

        apply_pdc_updates(&shared.pdc, &shared.plans);
        self.apply_directions(shared);

        // Frames resident across every streaming ring, sampled before the
        // refill so the outcome can say whether the refill actually achieved
        // anything. See [`StepOutcome::Stalled`].
        let buffered_before = total_buffered(&shared.plans, &self.local.regions);

        if self.config.parallel_io && shared.plans.len() >= 3 {
            refill_all_parallel(
                &shared.plans,
                &mut self.local.regions,
                &shared.metrics,
                self.config.chunk_size,
                self.local.buffer_margin,
            );
        } else {
            refill_all(
                &shared.plans,
                &mut self.local.regions,
                &shared.metrics,
                self.config.chunk_size,
                self.local.buffer_margin,
                &mut self.local.interleave_buffer,
            );
        }

        if buffers_healthy(&shared.plans, self.local.buffer_margin) {
            StepOutcome::Healthy
        } else if total_buffered(&shared.plans, &self.local.regions) > buffered_before {
            StepOutcome::Busy
        } else {
            StepOutcome::Stalled
        }
    }

    /// Turn each ring whose channel changed direction (`RtState`'s, set by the
    /// voice or by `SetVarispeed`): reverse is a mapping (the file mirrored,
    /// loop ignored), so the change is `apply_mapping`'s — a switch the reader
    /// crossfades across just past the block it is in.
    ///
    /// The plans are read and released before any ring is touched, since a
    /// switch reads the file for its fade.
    fn apply_directions(&mut self, shared: &Handles) {
        let turned: Vec<(super::command::RegionId, bool, f64)> = shared
            .plans
            .iter()
            .filter_map(|entry| {
                let plan = entry.value();
                let link = plan.link.as_ref()?;
                Some((
                    link.region_id,
                    plan.rt_state.is_reverse(),
                    plan.rt_state.read_rate().get(),
                ))
            })
            .collect();
        for (region_id, reverse, rate) in turned {
            let Some(writer) = self.local.regions.get_mut(region_id) else {
                continue;
            };
            if writer.content().current.reverse == reverse {
                continue;
            }
            let new = Mapping {
                reverse,
                ..writer.content().current.clone()
            };
            apply_mapping(writer, new, self.config.seek_crossfade_frames, rate);
        }
    }
}

/// Frames currently resident across every streaming ring.
///
/// Summed rather than compared per channel because the question it answers is
/// about the cycle as a whole: did *any* refill land? A per-channel comparison
/// would report a stall for a stream at EOF even while its neighbours were
/// still filling, and the pacing layer must keep running for those.
fn total_buffered(
    plans: &dashmap::DashMap<usize, super::plan::ChannelPlan>,
    regions: &super::region_map::RegionMap,
) -> usize {
    plans
        .iter()
        .filter_map(|entry| {
            let link = entry.value().link.as_ref()?;
            let writer = regions.get(link.region_id)?;
            Some(writer.buffered().get())
        })
        .sum()
}

/// True when no stream needs a refill — i.e. the loop can safely park on a
/// short timer instead of spinning.
fn buffers_healthy(
    plans: &dashmap::DashMap<usize, super::plan::ChannelPlan>,
    buffer_margin: f64,
) -> bool {
    let fill_threshold = (0.75 / buffer_margin) as f32;

    // Every streaming channel must be at or above its refill threshold. A
    // channel with no active link imposes no refill work.
    plans.iter().all(|entry| {
        let plan = entry.value();
        if plan.link.is_none() {
            return true;
        }
        plan.rt_state.buffer_fill() >= fill_threshold
    })
}
