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
use tutti_core::ArcLend;
use tutti_core::Samples;

/// The butler's read state on the published compensation table.
///
/// Built on the control thread when the table is subscribed
/// (`ButlerThread::with_pdc`) and owned by the butler's cycle state, which
/// reads through it by `&mut`: so a read state that must be registered on the
/// table before the butler runs (the coming `arc-lend` `Reader`) has a home
/// built off the butler thread, and one owner. A clone is another read state
/// on the same table, for a second cycle (a restarted butler, or one stepped
/// by hand).
///
/// Covered by `live_loop`'s `an_entry_past_the_loop_and_a_preroll_land_where_
/// the_memory_tier_plays`, a hand-stepped butler whose cycle is built by the
/// same `ButlerThread::cycle` the thread's is. Mutations (run): `with_pdc`
/// not keeping the feed → fails; a stream opened at preroll 0 instead of its
/// channel's entry (`handle_stream_file`) → fails.
#[derive(Clone, Default)]
pub(crate) struct PdcFeed {
    /// `None` = no PDC wiring: every channel pre-rolls 0.
    table: Option<Arc<ArcLend<Vec<Samples>>>>,
}

impl PdcFeed {
    /// A feed reading `table`. Control thread.
    pub(crate) fn over(table: Arc<ArcLend<Vec<Samples>>>) -> Self {
        Self { table: Some(table) }
    }

    /// `channel_index`'s preroll in the table now: 0 with no table, or past
    /// its end. Butler thread.
    pub(crate) fn preroll(&mut self, channel_index: usize) -> u64 {
        self.table.as_ref().map_or(0, |table| {
            table
                .lend()
                .get(channel_index)
                .copied()
                .unwrap_or_default()
                .get() as u64
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
/// A no-op when nothing is subscribed. Butler thread: it takes one
/// [`ArcLend::lend`] snapshot for the pass rather than per channel, and it
/// must never run on the audio thread.
pub(crate) fn apply_pdc_updates(pdc: &mut PdcFeed, plans: &DashMap<usize, ChannelPlan>) {
    let Some(table) = pdc.table.as_ref() else {
        return;
    };

    let snapshot = table.lend();

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::command::RegionId;
    use crate::butler::prefetch::{RegionBuffer, SharedReader};
    use std::path::PathBuf;

    /// A published compensation table, as `latency::compensate` would produce.
    fn table(compensations: [usize; 3]) -> Arc<ArcLend<Vec<Samples>>> {
        Arc::new(ArcLend::new(
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
        apply_pdc_updates(&mut PdcFeed::default(), &plans);
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
    #[test]
    fn each_ring_is_told_its_channels_preroll() {
        let (plans, rings) = streaming(4);
        let pdc = table([200, 0, 100]);
        let mut feed = PdcFeed::over(Arc::clone(&pdc));
        apply_pdc_updates(&mut feed, &plans);
        assert_eq!(
            rings.iter().map(|r| r.preroll()).collect::<Vec<_>>(),
            [200, 0, 100, 0]
        );
        assert_eq!(plans.get(&0).unwrap().pdc_preroll, 200);

        pdc.publish(Arc::new(vec![Samples(0), Samples(0), Samples(100)]));
        apply_pdc_updates(&mut feed, &plans);
        assert_eq!(rings[0].preroll(), 0);
        pdc.publish(Arc::new(vec![Samples(200)]));
        apply_pdc_updates(&mut feed, &plans);
        assert_eq!(rings[0].preroll(), 200);
        assert_eq!(rings[2].preroll(), 0, "past the table's end is no preroll");
    }
}
