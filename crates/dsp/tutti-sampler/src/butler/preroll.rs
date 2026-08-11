//! Stream preroll: the butler's half of delay compensation.
//!
//! A source outside the audio graph cannot be delayed by a node inside it —
//! instead it seeks its read head earlier, so its audio arrives already aligned.
//!
//! Named `preroll`, not `pdc`, to keep the split with `tutti_types::latency`
//! visible: that computes *how much* compensation each channel needs; this
//! consumes the answer and moves read heads. Not `io/` either — it repositions
//! streams, it does not read them.

use super::cache::LruCache;
use super::config::BufferConfig;
use super::loops::{fadein_samples, fadeout_samples};
use super::metrics::Metrics;
use super::plan::ChannelPlan;
use super::region_map::RegionMap;
use dashmap::DashMap;
use std::sync::Arc;
use tutti_core::RtPublish;
use tutti_core::Samples;

/// Reconcile every streaming channel's read head against the published
/// compensation table, once per refill cycle.
///
/// A channel whose entry has changed is repositioned by the delta through
/// [`reposition_click_free`]; a larger preroll seeks backward, a smaller one
/// forward. Channels whose compensation is unchanged, that are not streaming, or
/// that fall beyond the end of the table are left alone — a missing entry reads
/// as zero compensation, which is the same answer as "none needed".
///
/// A no-op when nothing is subscribed. Butler thread: it takes an
/// [`RtPublish::read`] snapshot for the pass rather than per channel, and it
/// must never run on the audio thread.
pub(crate) fn apply_pdc_updates(
    pdc: &Option<Arc<RtPublish<Vec<Samples>>>>,
    plans: &DashMap<usize, ChannelPlan>,
    regions: &mut RegionMap,
    cache: &LruCache,
    metrics: &Metrics,
    config: &BufferConfig,
) {
    let Some(pdc) = pdc.as_ref() else {
        return;
    };

    let snapshot = pdc.read();

    for mut entry in plans.iter_mut() {
        let channel_index = *entry.key();
        let stream_state = entry.value_mut();

        let current_preroll = stream_state.pdc_preroll;
        let new_preroll = snapshot
            .get(channel_index)
            .copied()
            .unwrap_or_default()
            .get() as u64;

        if new_preroll == current_preroll {
            continue;
        }

        let Some(link) = stream_state.link.as_ref() else {
            continue;
        };

        let Some(writer) = regions.get_mut(link.region_id) else {
            continue;
        };

        let current_pos = writer.file_position();
        let new_pos = pdc_new_position(current_pos, current_preroll, new_preroll);

        reposition_click_free(stream_state, writer, new_pos, cache, metrics, config);

        stream_state.pdc_preroll = new_preroll;
    }
}

/// Move a live stream's read head to `new_pos` without a click.
///
/// Capture the fadeout tail at the current position, flush the ring, seek the
/// writer, capture the fadein head at the new position, and hand both to the
/// audio thread's seek crossfader. The `seeking` flag brackets the whole move so
/// the audio thread mutes rather than reading a half-repositioned stream.
///
/// The two callers differ only in how they choose `new_pos` — a PDC preroll
/// delta here, an explicit timeline target in `handle_seek_stream` — so the move
/// itself lives once.
pub(in crate::butler) fn reposition_click_free(
    plan: &ChannelPlan,
    writer: &mut super::prefetch::RegionOut,
    new_pos: u64,
    cache: &LruCache,
    metrics: &Metrics,
    config: &BufferConfig,
) {
    let crossfade_len = config.seek_crossfade_frames;
    let ch = writer.channels();
    let fadeout = fadeout_samples(plan, cache, metrics, writer.file_path(), crossfade_len, ch);

    plan.set_seeking(true);
    plan.flush_buffer();
    writer.set_file_position(new_pos);

    let fadein = fadein_samples(
        cache,
        metrics,
        writer.file_path(),
        new_pos,
        crossfade_len,
        ch,
    );

    if !fadeout.is_empty() && !fadein.is_empty() {
        plan.rt_state.start_seek_crossfade(fadeout, fadein, ch);
    }

    plan.set_seeking(false);
}

/// New file position after a preroll change. A larger preroll seeks backward
/// (saturating at 0); a smaller preroll seeks forward by the same delta.
fn pdc_new_position(current: u64, old_preroll: u64, new_preroll: u64) -> u64 {
    if new_preroll > old_preroll {
        current.saturating_sub(new_preroll - old_preroll)
    } else {
        current + (old_preroll - new_preroll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::command::RegionId;
    use crate::butler::prefetch::RegionBuffer;
    use std::path::PathBuf;

    fn region(i: usize) -> RegionId {
        RegionId(i as u64 + 1)
    }

    /// A published compensation table, as `latency::compensate` would produce.
    fn table(compensations: [usize; 3]) -> Arc<RtPublish<Vec<Samples>>> {
        Arc::new(RtPublish::new(
            compensations.into_iter().map(Samples).collect(),
        ))
    }

    /// `tracks` is a count of **timeline tracks**, not audio channels.
    ///
    /// `channel_index` throughout `butler/` names a track slot in the PDC
    /// compensation table — one `ChannelPlan`, one region, one file — which is
    /// why every ring below is built stereo regardless of `tracks`. This
    /// deliberately does **not** take a [`ChannelLayout`](tutti_core::ChannelLayout):
    /// typing it as one would make a three-track fixture read as a 3-channel
    /// audio format, exactly the confusion the layout type exists to end.
    /// The genuine audio-channel use in this file is `writer.channels()`.
    fn create_test_fixtures(
        tracks: usize,
    ) -> (
        DashMap<usize, ChannelPlan>,
        RegionMap,
        LruCache,
        Metrics,
        BufferConfig,
    ) {
        let plans = DashMap::new();
        let mut regions = RegionMap::new();

        for i in 0..tracks {
            let region_id = region(i);
            let (writer, reader) =
                RegionBuffer::with_capacity(region_id, PathBuf::from("test.wav"), 4096, 2usize);
            regions.register(region_id, writer);

            let mut state = ChannelPlan::default();
            state.start_streaming(crate::butler::share_reader(reader), None);
            plans.insert(i, state);
        }

        (
            plans,
            regions,
            LruCache::new(10, 1024 * 1024),
            Metrics::new(),
            BufferConfig::default(),
        )
    }

    fn file_pos(regions: &RegionMap, channel: usize) -> u64 {
        regions.get(region(channel)).unwrap().file_position()
    }

    #[test]
    fn test_pdc_new_position_directions_and_saturation() {
        // Preroll increased -> seek backward by the delta.
        assert_eq!(pdc_new_position(1000, 0, 500), 500);
        // Preroll decreased -> seek forward by the delta.
        assert_eq!(pdc_new_position(500, 500, 0), 1000);
        // Backward seek saturates at 0 instead of underflowing.
        assert_eq!(pdc_new_position(100, 0, 500), 0);
    }

    #[test]
    fn test_no_pdc_subscription_is_noop() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(1);

        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        apply_pdc_updates(&None, &plans, &mut regions, &cache, &metrics, &config);

        assert_eq!(file_pos(&regions, 0), 1000);
    }

    #[test]
    fn test_preroll_unchanged_no_seek() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(1);
        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        let pdc = table([0, 0, 0]);
        apply_pdc_updates(&Some(pdc), &plans, &mut regions, &cache, &metrics, &config);

        assert_eq!(file_pos(&regions, 0), 1000);
    }

    #[test]
    fn test_preroll_increased_seeks_backward() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(2);
        regions.get_mut(region(0)).unwrap().set_file_position(1000);
        regions.get_mut(region(1)).unwrap().set_file_position(1000);

        // Channel 0 must pre-roll 500; channel 1 is already the worst case.
        let pdc = table([500, 0, 0]);
        apply_pdc_updates(&Some(pdc), &plans, &mut regions, &cache, &metrics, &config);

        assert_eq!(file_pos(&regions, 0), 500);
        assert_eq!(file_pos(&regions, 1), 1000);

        assert_eq!(plans.get(&0).unwrap().pdc_preroll, 500);
        assert_eq!(plans.get(&1).unwrap().pdc_preroll, 0);
    }

    #[test]
    fn test_preroll_decreased_seeks_forward() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(1);
        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        let pdc = table([500, 0, 0]);
        apply_pdc_updates(
            &Some(Arc::clone(&pdc)),
            &plans,
            &mut regions,
            &cache,
            &metrics,
            &config,
        );

        assert_eq!(plans.get(&0).unwrap().pdc_preroll, 500);
        assert_eq!(file_pos(&regions, 0), 500);

        // The latency goes away — republish, and the read head seeks back forward.
        pdc.publish(Arc::new(vec![Samples(0)]));
        apply_pdc_updates(&Some(pdc), &plans, &mut regions, &cache, &metrics, &config);

        assert_eq!(file_pos(&regions, 0), 1000);
        assert_eq!(plans.get(&0).unwrap().pdc_preroll, 0);
    }

    #[test]
    fn test_seeking_flag_clear_after_update() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(1);
        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        assert!(!plans.get(&0).unwrap().rt_state.is_seeking());

        let pdc = table([500, 0, 0]);
        apply_pdc_updates(&Some(pdc), &plans, &mut regions, &cache, &metrics, &config);

        assert!(!plans.get(&0).unwrap().rt_state.is_seeking());
    }

    #[test]
    fn test_multiple_channels_independent_compensation() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(3);
        for i in 0..3 {
            regions.get_mut(region(i)).unwrap().set_file_position(1000);
        }

        let pdc = table([200, 0, 100]);
        apply_pdc_updates(&Some(pdc), &plans, &mut regions, &cache, &metrics, &config);

        assert_eq!(file_pos(&regions, 0), 800);
        assert_eq!(plans.get(&0).unwrap().pdc_preroll, 200);

        assert_eq!(file_pos(&regions, 1), 1000);
        assert_eq!(plans.get(&1).unwrap().pdc_preroll, 0);

        assert_eq!(file_pos(&regions, 2), 900);
        assert_eq!(plans.get(&2).unwrap().pdc_preroll, 100);
    }

    #[test]
    fn test_channel_beyond_table_is_uncompensated() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(1);
        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        // Empty table — channel 0 has no entry.
        let pdc = Arc::new(RtPublish::new(Vec::new()));
        apply_pdc_updates(&Some(pdc), &plans, &mut regions, &cache, &metrics, &config);

        assert_eq!(file_pos(&regions, 0), 1000);
    }
}
