//! Plugin delay compensation (PDC) integration for butler thread.

use super::super::cache::LruCache;
use super::super::config::BufferConfig;
use super::super::metrics::Metrics;
use super::super::plan::ChannelPlan;
use super::super::region_map::RegionMap;
use super::loops::{fadein_samples, fadeout_samples};
use arc_swap::ArcSwap;
use dashmap::DashMap;
use std::sync::Arc;
use tutti_core::PdcState;

/// Called each refill cycle. Detects plugin latency changes and
/// adjusts stream positions with smooth crossfades.
pub(crate) fn apply_pdc_updates(
    pdc: &Option<Arc<ArcSwap<PdcState>>>,
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
    if !snapshot.enabled() {
        return;
    }

    for mut entry in plans.iter_mut() {
        let channel_index = *entry.key();
        let stream_state = entry.value_mut();

        let current_preroll = stream_state.pdc_preroll;
        let new_preroll = snapshot
            .channel_compensations()
            .get(channel_index)
            .copied()
            .unwrap_or(0) as u64;

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
        let new_pos = if new_preroll > current_preroll {
            current_pos.saturating_sub(new_preroll - current_preroll)
        } else {
            current_pos + (current_preroll - new_preroll)
        };

        let crossfade_len = config.seek_crossfade_samples;
        let fadeout = fadeout_samples(stream_state, crossfade_len);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::command::RegionId;
    use crate::butler::prefetch::RegionBuffer;
    use std::path::PathBuf;
    use tutti_core::PdcManager;

    fn region(i: usize) -> RegionId {
        RegionId(i as u64 + 1)
    }

    fn create_test_fixtures(
        channel_count: usize,
    ) -> (
        DashMap<usize, ChannelPlan>,
        RegionMap,
        LruCache,
        Metrics,
        BufferConfig,
    ) {
        let plans = DashMap::new();
        let mut regions = RegionMap::new();

        for i in 0..channel_count {
            let region_id = region(i);
            let (writer, reader) =
                RegionBuffer::with_capacity(region_id, PathBuf::from("test.wav"), 4096);
            regions.register(region_id, writer);

            let mut state = ChannelPlan::default();
            state.start_streaming(Arc::new(parking_lot::Mutex::new(reader)));
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

    /// Build a PDC subscription from a `PdcManager` (used as test-setup helper).
    fn pdc_sub(mgr: &PdcManager) -> Arc<ArcSwap<PdcState>> {
        mgr.snapshot_arc()
    }

    #[test]
    fn test_no_pdc_subscription_is_noop() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(1);

        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        apply_pdc_updates(&None, &plans, &mut regions, &cache, &metrics, &config);

        assert_eq!(file_pos(&regions, 0), 1000);
    }

    #[test]
    fn test_pdc_disabled_is_noop() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(1);

        let mgr = PdcManager::new(4, 0);
        mgr.set_channel_latency(0, 500);
        mgr.set_enabled(false);

        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        apply_pdc_updates(
            &Some(pdc_sub(&mgr)),
            &plans,
            &mut regions,
            &cache,
            &metrics,
            &config,
        );

        assert_eq!(file_pos(&regions, 0), 1000);
    }

    #[test]
    fn test_preroll_unchanged_no_seek() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(1);

        let mgr = PdcManager::new(4, 0);

        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        apply_pdc_updates(
            &Some(pdc_sub(&mgr)),
            &plans,
            &mut regions,
            &cache,
            &metrics,
            &config,
        );

        assert_eq!(file_pos(&regions, 0), 1000);
    }

    #[test]
    fn test_preroll_increased_seeks_backward() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(2);

        let mgr = PdcManager::new(4, 0);
        mgr.set_channel_latency(1, 500);

        regions.get_mut(region(0)).unwrap().set_file_position(1000);
        regions.get_mut(region(1)).unwrap().set_file_position(1000);

        apply_pdc_updates(
            &Some(pdc_sub(&mgr)),
            &plans,
            &mut regions,
            &cache,
            &metrics,
            &config,
        );

        // Channel 0 gets 500 compensation -> seeks back to 500.
        assert_eq!(file_pos(&regions, 0), 500);
        // Channel 1 is the max latency -> no compensation.
        assert_eq!(file_pos(&regions, 1), 1000);

        assert_eq!(plans.get(&0).unwrap().pdc_preroll, 500);
        assert_eq!(plans.get(&1).unwrap().pdc_preroll, 0);
    }

    #[test]
    fn test_preroll_decreased_seeks_forward() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(1);

        let mgr = PdcManager::new(4, 0);
        mgr.set_channel_latency(1, 500);
        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        apply_pdc_updates(
            &Some(pdc_sub(&mgr)),
            &plans,
            &mut regions,
            &cache,
            &metrics,
            &config,
        );

        assert_eq!(plans.get(&0).unwrap().pdc_preroll, 500);
        assert_eq!(file_pos(&regions, 0), 500);

        mgr.remove_channel(1);

        apply_pdc_updates(
            &Some(pdc_sub(&mgr)),
            &plans,
            &mut regions,
            &cache,
            &metrics,
            &config,
        );

        assert_eq!(file_pos(&regions, 0), 1000);
        assert_eq!(plans.get(&0).unwrap().pdc_preroll, 0);
    }

    #[test]
    fn test_seeking_flag_set_during_update() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(1);

        let mgr = PdcManager::new(4, 0);
        mgr.set_channel_latency(1, 500);

        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        assert!(!plans.get(&0).unwrap().rt_state.is_seeking());

        apply_pdc_updates(
            &Some(pdc_sub(&mgr)),
            &plans,
            &mut regions,
            &cache,
            &metrics,
            &config,
        );

        assert!(!plans.get(&0).unwrap().rt_state.is_seeking());
    }

    #[test]
    fn test_multiple_channels_independent_compensation() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(3);

        let mgr = PdcManager::new(4, 0);
        mgr.set_channel_latency(0, 100);
        mgr.set_channel_latency(1, 300);
        mgr.set_channel_latency(2, 200);

        regions.get_mut(region(0)).unwrap().set_file_position(1000);
        regions.get_mut(region(1)).unwrap().set_file_position(1000);
        regions.get_mut(region(2)).unwrap().set_file_position(1000);

        apply_pdc_updates(
            &Some(pdc_sub(&mgr)),
            &plans,
            &mut regions,
            &cache,
            &metrics,
            &config,
        );

        assert_eq!(file_pos(&regions, 0), 800);
        assert_eq!(plans.get(&0).unwrap().pdc_preroll, 200);

        assert_eq!(file_pos(&regions, 1), 1000);
        assert_eq!(plans.get(&1).unwrap().pdc_preroll, 0);

        assert_eq!(file_pos(&regions, 2), 900);
        assert_eq!(plans.get(&2).unwrap().pdc_preroll, 100);
    }

    #[test]
    fn test_channel_not_in_pdc_snapshot() {
        let (plans, mut regions, cache, metrics, config) = create_test_fixtures(1);

        let mgr = PdcManager::new(0, 0);
        regions.get_mut(region(0)).unwrap().set_file_position(1000);

        apply_pdc_updates(
            &Some(pdc_sub(&mgr)),
            &plans,
            &mut regions,
            &cache,
            &metrics,
            &config,
        );

        assert_eq!(file_pos(&regions, 0), 1000);
    }
}
