//! Stream preroll: the butler's half of delay compensation.
//!
//! A source outside the audio graph cannot be delayed by a node inside it —
//! instead it reads earlier, so its audio arrives already aligned.
//!
//! Named `preroll`, not `pdc`, to keep the split with `tutti_types::latency`
//! visible: that computes *how much* compensation each channel needs; this
//! consumes the answer. Not `io/` either — it moves no frames.
//!
//! # A change moves the reader, and the butler follows
//!
//! The preroll is published on the stream's ring (`Ring::preroll`); the reader
//! plays straight position `clock - preroll` (a free-running reader, its own
//! position less it). A change is therefore a jump of the *reader's* position
//! by the delta, from where it plays: the reader crossfades across it, and the
//! refill follows it (`io::refill`), as it follows any jump. The frames the
//! ring already holds stay valid for their positions, so a change inside the
//! window costs no refill at all. (Moving the *writer's* cursor by the delta
//! and flushing instead would skip whatever lay buffered between the two.)

use super::plan::ChannelPlan;
use dashmap::DashMap;
use std::sync::Arc;
use tutti_core::PrerollTable;

/// The butler's handle on the published compensation table, if any.
///
/// Built for each cycle (`ButlerThread::cycle`, and `ButlerThread::with_pdc`
/// for a cycle already stepped by hand) from the subscription the controller
/// keeps.
///
/// Covered by `live_loop`'s `an_entry_past_the_loop_and_a_preroll_land_where_
/// the_memory_tier_plays`, a hand-stepped butler whose cycle is built by the
/// same `ButlerThread::cycle` the thread's is. Mutations (run): `with_pdc`
/// not keeping the feed → fails; a stream opened at preroll 0 instead of its
/// channel's entry (`handle_stream_file`) → fails.
#[derive(Default)]
pub(crate) struct PdcFeed {
    /// `None` = no PDC wiring: every channel pre-rolls 0.
    table: Option<Arc<PrerollTable>>,
}

impl PdcFeed {
    /// A feed reading `table`, or none.
    pub(crate) fn over(table: Option<&Arc<PrerollTable>>) -> Self {
        Self {
            table: table.map(Arc::clone),
        }
    }

    /// `channel_index`'s preroll in the table now: 0 with no table, or past
    /// its end. Butler thread.
    pub(crate) fn preroll(&self, channel_index: usize) -> u64 {
        self.table.as_ref().map_or(0, |table| {
            table.preroll(channel_index).unwrap_or_default().get() as u64
        })
    }
}

/// Reconcile every streaming channel's preroll against the published
/// compensation table, once per refill cycle.
///
/// A channel whose entry has changed publishes the new preroll to its ring.
/// Channels whose compensation is unchanged, that are not streaming, or that
/// fall beyond the end of the table are left alone — a missing entry reads as
/// zero compensation, which is the same answer as "none needed".
///
/// A no-op when nothing is subscribed. Butler thread: it reads the table
/// under one lock for the pass rather than per channel, and it must never
/// run on the audio thread.
pub(crate) fn apply_pdc_updates(pdc: &PdcFeed, plans: &DashMap<usize, ChannelPlan>) {
    let Some(table) = pdc.table.as_ref() else {
        return;
    };
    // The one lock the pass takes while it walks `plans`; nothing that
    // holds a `plans` entry takes it (`set` is the session's, which touches
    // no plan), so the two never wait on each other.
    table.with(|snapshot| {
        for mut entry in plans.iter_mut() {
            let channel_index = *entry.key();
            let plan = entry.value_mut();

            let preroll = snapshot
                .get(channel_index)
                .copied()
                .unwrap_or_default()
                .get() as u64;
            if preroll == plan.pdc_preroll {
                continue;
            }
            let Some(link) = plan.link.as_ref() else {
                continue;
            };
            link.consumer.set_preroll(preroll);
            plan.pdc_preroll = preroll;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::command::RegionId;
    use crate::butler::prefetch::{RegionBuffer, SharedReader};
    use std::path::PathBuf;
    use tutti_core::Samples;

    /// A published compensation table, as `latency::compensate` would produce.
    fn table(compensations: [usize; 3]) -> Arc<PrerollTable> {
        Arc::new(PrerollTable::new(
            compensations.into_iter().map(Samples).collect(),
        ))
    }

    /// `tracks` is a count of **timeline tracks**, not audio channels: one
    /// `ChannelPlan`, one ring, one file each.
    fn streaming(tracks: usize) -> (DashMap<usize, ChannelPlan>, Vec<SharedReader>) {
        let plans = DashMap::new();
        let mut rings = Vec::new();
        for i in 0..tracks {
            let (_writer, ring) = RegionBuffer::with_capacity(
                RegionId(i as u64 + 1),
                PathBuf::from("test.wav"),
                4096,
                2usize,
            );
            let mut state = ChannelPlan::default();
            state.start_streaming(
                Arc::clone(&ring),
                None,
                tutti_core::SampleRate::SR_48K,
                48_000,
                PathBuf::from("test.wav"),
                std::sync::Weak::new(),
            );
            plans.insert(i, state);
            rings.push(ring);
        }
        (plans, rings)
    }

    #[test]
    fn with_nothing_subscribed_nothing_moves() {
        let (plans, rings) = streaming(1);
        apply_pdc_updates(&PdcFeed::default(), &plans);
        assert_eq!(rings[0].preroll(), 0);
    }

    /// **Each channel's ring is told its own preroll, and a change is told
    /// again**: channel 0 pre-rolls 200, channel 2 100, channel 1 none; a
    /// republished table with channel 0 back at 0 reaches it; a channel past
    /// the table's end reads 0.
    ///
    /// Mutation (run): the ring not told (`set_preroll` dropped) → fails.
    /// Mutation (run): the plan's figure not updated → a later change back to
    /// the old figure is skipped → fails.
    /// Mutation (run): the feed holding a copy of the table, not the shared
    /// one → the republished table never reaches channel 0 → fails.
    #[test]
    fn each_ring_is_told_its_channels_preroll() {
        let (plans, rings) = streaming(4);
        let pdc = table([200, 0, 100]);
        let feed = PdcFeed::over(Some(&pdc));
        apply_pdc_updates(&feed, &plans);
        assert_eq!(
            rings.iter().map(|r| r.preroll()).collect::<Vec<_>>(),
            [200, 0, 100, 0]
        );
        assert_eq!(plans.get(&0).unwrap().pdc_preroll, 200);

        pdc.set(&[Samples(0), Samples(0), Samples(100)]);
        apply_pdc_updates(&feed, &plans);
        assert_eq!(rings[0].preroll(), 0);
        pdc.set(&[Samples(200)]);
        apply_pdc_updates(&feed, &plans);
        assert_eq!(rings[0].preroll(), 200);
        assert_eq!(rings[2].preroll(), 0, "past the table's end is no preroll");
    }
}
