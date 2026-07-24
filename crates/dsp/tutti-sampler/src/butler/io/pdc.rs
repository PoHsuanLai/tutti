//! Delay compensation for streaming playback.
//!
//! A source outside the audio graph cannot be delayed by a node inside it —
//! instead it seeks its read head earlier, so its audio arrives already aligned.

use super::super::cache::LruCache;
use super::super::config::BufferConfig;
use super::super::metrics::Metrics;
use super::super::plan::ChannelPlan;
use super::super::region_map::RegionMap;
use super::loops::{fadein_samples, fadeout_samples};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use std::sync::Arc;
use tutti_core::Samples;

/// Called each refill cycle. Detects compensation changes and adjusts stream
/// positions with smooth crossfades.
pub(crate) fn apply_pdc_updates(
    pdc: &Option<Arc<ArcSwap<Vec<Samples>>>>,
    plans: &DashMap<usize, ChannelPlan>,
    regions: &mut RegionMap,
    cache: &LruCache,
    metrics: &Metrics,
    config: &BufferConfig,
) {
    let Some(pdc) = pdc.as_ref() else {
        return;
    };

    let snapshot = pdc.load_full();

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

        let crossfade_len = config.seek_crossfade_samples;
        let fadeout = fadeout_samples(
            stream_state,
            cache,
            metrics,
            writer.file_path(),
            crossfade_len,
        );

        stream_state.set_seeking(true);
        stream_state.flush_buffer();
        writer.set_file_position(new_pos);

        let fadein = fadein_samples(cache, metrics, writer.file_path(), new_pos, crossfade_len);

        if !fadeout.is_empty() && !fadein.is_empty() {
            stream_state.rt_state.start_seek_crossfade(fadeout, fadein);
        }

        stream_state.pdc_preroll = new_preroll;
        stream_state.set_seeking(false);
    }
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
    use tutti_core::ChannelLayout;

    fn region(i: usize) -> RegionId {
        RegionId(i as u64 + 1)
    }

    /// A published compensation table, as `latency::compensate` would produce.
    fn table(compensations: [usize; 3]) -> Arc<ArcSwap<Vec<Samples>>> {
        Arc::new(ArcSwap::from_pointee(
            compensations.into_iter().map(Samples).collect(),
        ))
    }

    fn create_test_fixtures(
        layout: ChannelLayout,
    ) -> (
        DashMap<usize, ChannelPlan>,
        RegionMap,
        LruCache,
        Metrics,
        BufferConfig,
    ) {
        let plans = DashMap::new();
        let mut regions = RegionMap::new();

        for i in 0..layout.count() as usize {
            let region_id = region(i);
            let (writer, reader) =
                RegionBuffer::with_capacity(region_id, PathBuf::from("test.wav"), 4096);
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
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(ChannelLayout::from(1usize));

        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        apply_pdc_updates(&None, &plans, &mut regions, &cache, &metrics, &config);

        assert_eq!(file_pos(&regions, 0), 1000);
    }

    #[test]
    fn test_preroll_unchanged_no_seek() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(ChannelLayout::from(1usize));
        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        let pdc = table([0, 0, 0]);
        apply_pdc_updates(&Some(pdc), &plans, &mut regions, &cache, &metrics, &config);

        assert_eq!(file_pos(&regions, 0), 1000);
    }

    #[test]
    fn test_preroll_increased_seeks_backward() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(ChannelLayout::from(2usize));
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
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(ChannelLayout::from(1usize));
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
        pdc.store(Arc::new(vec![Samples(0)]));
        apply_pdc_updates(&Some(pdc), &plans, &mut regions, &cache, &metrics, &config);

        assert_eq!(file_pos(&regions, 0), 1000);
        assert_eq!(plans.get(&0).unwrap().pdc_preroll, 0);
    }

    #[test]
    fn test_seeking_flag_clear_after_update() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(ChannelLayout::from(1usize));
        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        assert!(!plans.get(&0).unwrap().rt_state.is_seeking());

        let pdc = table([500, 0, 0]);
        apply_pdc_updates(&Some(pdc), &plans, &mut regions, &cache, &metrics, &config);

        assert!(!plans.get(&0).unwrap().rt_state.is_seeking());
    }

    #[test]
    fn test_multiple_channels_independent_compensation() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(ChannelLayout::from(3usize));
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
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(ChannelLayout::from(1usize));
        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        // Empty table — channel 0 has no entry.
        let pdc = Arc::new(ArcSwap::from_pointee(Vec::new()));
        apply_pdc_updates(&Some(pdc), &plans, &mut regions, &cache, &metrics, &config);

        assert_eq!(file_pos(&regions, 0), 1000);
    }
}
