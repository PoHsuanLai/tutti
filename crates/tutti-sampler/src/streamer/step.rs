//! One streamer cycle, synchronously.
//!
//! The streamer's work is a loop over one step after its commands: refill the
//! rings (following each reader to where it plays). It is not asynchronous: it
//! walks the stream table, decodes from a file, and writes into a ring. The only
//! async in the streamer is the *pacing* — the timer the loop parks on when
//! there is nothing urgent to do. (What a *command* does to the table is the
//! pure [`step`](super::record::step), not this.)
//!
//! This module is that work with the pacing removed. [`StreamerCycle::step`]
//! drains the command queue, runs the refill, and reports what the pacing
//! layer should do next as a [`StepOutcome`]. The async loop in
//! [`loop_body`](super::loop_body) is then the pacing layer and nothing else.
//!
//! # Why the split earns its name
//!
//! A test that has to observe a streamer-driven stream otherwise has only wall
//! clock to work with: send a command, sleep, look, sleep again. That is not a
//! stylistic complaint. The streamer parks 1 ms when idle and 3 ms when every
//! ring is healthy, so a poller reading at 5–10 ms is racing a producer it
//! cannot see, and every such test carries a timeout that is really a guess
//! about the machine it runs on. Driving the same steps directly makes the
//! question "has the streamer done the work" answerable by *counting* rather
//! than by waiting — `step` returns only after the work is done.
//!
//! It is the same seam the production loop uses, not a parallel test path:
//! `streamer_loop` is written in terms of `StreamerCycle::step`, so a
//! divergence between what tests drive and what ships is not representable.

use super::StreamerSender;
use super::command::StreamerCommand;
use super::config::BufferConfig;
use super::handlers::{Handles, Local, handle_command};
use super::io::refill::{FILL_THRESHOLD, refill_all, refill_all_parallel};
use super::record::Streams;

/// What the pacing layer should do after one streamer cycle.
///
/// The streamer runs at maximum thread priority, so "keep going" and "there is
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
    /// At least one ring is below its refill threshold and this cycle moved
    /// frames into one. Come straight back; there is more to do.
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
    /// It is also what a step-driven caller needs: "the streamer has done all it
    /// can" is a *fixed point*, and a caller that waited for `Healthy` instead
    /// would wait forever near the end of a file.
    Stalled,
    /// Shutdown was signalled. Stop.
    Shutdown,
}

/// The streamer's per-cycle state and the step over it.
///
/// Owns [`Local`] (streamer-thread-local: the stream table, scratch) and
/// borrows nothing. Both the async loop and a test driver build one
/// and call [`step`](Self::step) repeatedly; the only difference between them is
/// what happens *between* the calls.
pub(crate) struct StreamerCycle {
    local: Local,
    config: BufferConfig,
}

impl StreamerCycle {
    /// The stream table (tests).
    #[cfg(test)]
    pub(crate) fn streams(&self) -> &Streams {
        &self.local.streams
    }

    /// A cycle whose scratch is pre-sized for one `config.chunk_size` refill.
    ///
    /// `reports` is the streamer's own queue, which its decode thread answers
    /// on.
    pub(crate) fn new(config: BufferConfig, reports: StreamerSender) -> Self {
        Self {
            local: Local::new(config.chunk_size, reports),
            config,
        }
    }

    /// Run one cycle: drain every queued command, free every ring's retired
    /// controls, then — when any channel is streaming — refill the rings.
    ///
    /// A direction change is a command, applied in the drain, so the refill
    /// writes the new mapping this cycle.
    ///
    /// `drain` is called until it yields `None`; commands are applied
    /// non-blockingly, so a step never waits for one to arrive.
    pub(crate) fn step(
        &mut self,
        shared: &Handles,
        mut drain: impl FnMut() -> Option<StreamerCommand>,
    ) -> StepOutcome {
        while let Some(cmd) = drain() {
            if matches!(cmd, StreamerCommand::Shutdown) {
                handle_command(cmd, shared, &self.config, &mut self.local);
                return StepOutcome::Shutdown;
            }
            handle_command(cmd, shared, &self.config, &mut self.local);
        }

        // Every ring's retired controls, whether or not this cycle publishes
        // one: the streamer is their only publisher, and its cycle is the
        // control-side tick they get. A lock and a length check per ring
        // when nothing is retired.
        for record in self.local.streams.records() {
            record.out.ring().reclaim_control();
        }

        if self.local.streams.is_empty() {
            return StepOutcome::Idle;
        }

        // Frames resident across every streaming ring, sampled before the
        // refill so the outcome can say whether the refill actually achieved
        // anything. See [`StepOutcome::Stalled`].
        let buffered_before = total_buffered(&self.local.streams);

        if self.config.parallel_io && self.local.streams.len() >= 3 {
            refill_all_parallel(
                self.local.streams.records_mut(),
                &shared.metrics,
                self.config.chunk_size,
            );
        } else {
            refill_all(
                self.local.streams.records_mut(),
                &shared.metrics,
                self.config.chunk_size,
                &mut self.local.interleave_buffer,
            );
        }

        self.local.streams.reap();

        if buffers_healthy(&self.local.streams) {
            StepOutcome::Healthy
        } else if total_buffered(&self.local.streams) > buffered_before {
            StepOutcome::Busy
        } else {
            StepOutcome::Stalled
        }
    }
}

/// Frames currently resident across every live stream's ring.
///
/// Summed rather than compared per stream because the question it answers is
/// about the cycle as a whole: did *any* refill land? A per-stream comparison
/// would report a stall for a stream at EOF even while its neighbours were
/// still filling, and the pacing layer must keep running for those.
fn total_buffered(streams: &Streams) -> usize {
    streams
        .records()
        .iter()
        .filter(|r| r.is_live())
        .map(|r| r.out.buffered().get())
        .sum()
}

/// True when no stream needs a refill — i.e. the loop can safely park on a
/// short timer instead of spinning.
fn buffers_healthy(streams: &Streams) -> bool {
    // Every live stream must be at or above its refill threshold.
    streams
        .records()
        .iter()
        .filter(|r| r.is_live())
        .all(|r| r.fill() >= FILL_THRESHOLD)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use tutti_types::SampleRate;

    use super::super::cache::LruCache;
    use super::super::command::StreamId;
    use super::super::decode::Seams;
    use super::super::handlers::SessionRate;
    use super::super::loops::{Arrangement, RingMap};
    use super::super::metrics::Metrics;
    use super::super::record::{Cmd, step};
    use super::*;

    /// A `RingControl` the reader held across the streamer's last publish is
    /// freed by the streamer's next cycle, with no further publish, whether or
    /// not a stream is live (this one's reader was never delivered).
    ///
    /// Mutation (run): the cycle's per-ring `reclaim_control` loop removed → the
    /// control is still retired after the step, fails.
    #[test]
    fn a_cycle_frees_a_control_the_reader_held_across_its_replacement() {
        let config = BufferConfig::default();
        let shared = Handles {
            cache: Arc::new(LruCache::new(
                config.cache_max_entries,
                config.cache_max_bytes,
            )),
            metrics: Arc::new(Metrics::new()),
            session_rate: SessionRate::new(SampleRate(48_000.0)),
            seams: Seams::default(),
        };
        let (reports, _queue) = crossbeam_channel::bounded(8);
        let mut cycle = StreamerCycle::new(config, reports);
        let id = StreamId(1);
        let (record, ring, _pos) =
            super::super::record::tests::record(id, PathBuf::from("t.wav"), 1024, 1u16);
        let _ = step(&mut cycle.local.streams, Cmd::Open(Box::new(record)));

        let mut control = ring.control_reader();
        {
            // A block in progress across the publish.
            let _held = control.lend();
            ring.publish_map(RingMap::plain(Arrangement::plain(64), 1));
        }
        assert_eq!(ring.retired_controls(), 1, "pinned until a scan");
        // The stream is still `Opening` (its reader never delivered), so the
        // cycle refills nothing, but it still sweeps the ring's controls.
        let _ = cycle.step(&shared, || None);
        assert_eq!(ring.retired_controls(), 0, "freed by the cycle");
    }

    /// The same sweep frees a `RingControl` held across a *loop* publish
    /// (the applied loop replaces the control without a new map): the value
    /// the reader held is retired by the replacement and freed by the next
    /// cycle, with nothing published since.
    ///
    /// Mutation (run): the cycle's per-ring `reclaim_control` loop removed →
    /// the control is still retired after the step → fails. Mutation (run):
    /// `Ring::publish_loop` publishing nothing (the new loop dropped) → the
    /// first assertion fails.
    #[test]
    fn a_cycle_frees_a_control_the_reader_held_across_a_loop_publish() {
        let config = BufferConfig::default();
        let shared = Handles {
            cache: Arc::new(LruCache::new(
                config.cache_max_entries,
                config.cache_max_bytes,
            )),
            metrics: Arc::new(Metrics::new()),
            session_rate: SessionRate::new(SampleRate(48_000.0)),
            seams: Seams::default(),
        };
        let (reports, _queue) = crossbeam_channel::bounded(8);
        let mut cycle = StreamerCycle::new(config, reports);
        let (record, ring, _pos) =
            super::super::record::tests::record(StreamId(1), PathBuf::from("t.wav"), 1024, 1u16);
        let _ = step(&mut cycle.local.streams, Cmd::Open(Box::new(record)));

        let mut control = ring.control_reader();
        let applied = crate::voice::LoopSetting::On {
            start: tutti_types::SamplePosition(0.0),
            end: tutti_types::SamplePosition(64.0),
            crossfade_frames: 0,
        };
        {
            let held = control.lend();
            assert_eq!(held.loop_, crate::voice::LoopSetting::Off);
            ring.publish_loop(applied);
        }
        assert_eq!(ring.applied().loop_, applied, "the loop is published");
        assert_eq!(ring.retired_controls(), 1, "pinned until a scan");
        let _ = cycle.step(&shared, || None);
        assert_eq!(ring.retired_controls(), 0, "freed by the cycle");
        assert_eq!(control.lend().loop_, applied, "the reader sees the new one");
    }
}
