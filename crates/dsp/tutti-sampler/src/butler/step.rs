//! One butler cycle, synchronously.
//!
//! The butler's work is a loop over four pure steps — apply PDC preroll
//! changes, apply audio-thread seek requests, advance loop state, refill the
//! rings. None of them is asynchronous: they read a `DashMap`, decode from a
//! file, and push into a ring. The only async in the butler is the *pacing* —
//! the timer the loop parks on when there is nothing urgent to do.
//!
//! This module is that work with the pacing removed. [`ButlerCycle::step`]
//! drains the command queue, runs the four steps, and reports what the pacing
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
use super::handlers::{handle_command, handle_seek_stream, Handles, Local};
use super::io::refill::{refill_all, refill_all_parallel};
use super::loops::handle_loops;
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
    /// streaming — apply PDC preroll changes, apply audio-thread seek requests,
    /// advance loop state, and refill the rings.
    ///
    /// Seeks are applied *before* refill so the ring refills from the new offset
    /// in the same cycle. That ordering is the reason this is one function and
    /// not four public ones: a caller that ran them in a different order would
    /// refill from the pre-seek position and then discard it.
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

        apply_pdc_updates(
            &shared.pdc,
            &shared.plans,
            &mut self.local.regions,
            &shared.cache,
            &shared.metrics,
            &self.config,
        );

        // Audio-thread-requested timeline seeks: apply BEFORE refill so the
        // ring refills from the new disk offset this cycle.
        self.apply_seek_requests(shared);

        handle_loops(
            &shared.plans,
            &mut self.local.regions,
            &shared.cache,
            &shared.metrics,
        );

        // Frames resident across every streaming ring, sampled before the
        // refill so the outcome can say whether the refill actually achieved
        // anything. See [`StepOutcome::Stalled`].
        let buffered_before = total_buffered(&shared.plans, &self.local.regions);

        if self.config.parallel_io && shared.plans.len() >= 3 {
            refill_all_parallel(
                &shared.plans,
                &mut self.local.regions,
                &shared.cache,
                &shared.metrics,
                self.config.chunk_size,
                self.local.buffer_margin,
            );
        } else {
            refill_all(
                &shared.plans,
                &mut self.local.regions,
                &shared.cache,
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

    /// Apply any audio-thread-requested timeline seeks. For each streaming
    /// channel whose [`RtState`](super::rt_state::RtState) has a fresh seek
    /// request (epoch changed vs. the butler's last-applied), reposition the
    /// live stream to the requested absolute file offset via the same
    /// click-free path as PDC/loop reposition. Coalesces rapid seeks (only the
    /// latest target survives) — the desired behavior for scrubbing.
    ///
    /// Channel indices + targets are collected first so the plan refs are
    /// released before [`handle_seek_stream`] re-acquires them (avoids DashMap
    /// re-entrancy).
    fn apply_seek_requests(&mut self, shared: &Handles) {
        let mut pending: Vec<(usize, u64)> = Vec::new();
        for entry in shared.plans.iter() {
            let plan = entry.value();
            if plan.link.is_none() {
                continue;
            }
            if let Some(target) = plan.rt_state.take_seek_request() {
                pending.push((*entry.key(), target));
            }
        }

        for (channel_index, file_position) in pending {
            handle_seek_stream(
                channel_index,
                file_position,
                shared,
                &self.config,
                &mut self.local,
            );
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
