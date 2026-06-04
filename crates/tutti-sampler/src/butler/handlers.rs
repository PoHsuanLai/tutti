//! Butler command handlers + their state types.
//!
//! `Handles` — Arc'd state also held by `ButlerThread`.
//! `Local`    — butler-thread-local data (producers, captures, ...).
//! `handle_command` — dispatch; each arm is either inline or a `handle_*` fn.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use parking_lot::Mutex;
use tutti_core::{PdcState, Wave};

use super::cache::LruCache;
use super::command::{ButlerCommand, CaptureId, RegionId};
use super::config::BufferConfig;
use super::io::capture::{flush_all, flush_capture, open_wav, ActiveCapture};
use super::io::loops::buffer_size_for_file;
use super::metrics::Metrics;
use super::plan::ChannelPlan;
use super::prefetch::RegionBuffer;
use super::region_map::RegionMap;

/// Arc'd handles shared between `ButlerThread` (controller) and the butler
/// loop. `Clone` is cheap — every field is an `Arc` or `Option<Arc>`.
#[derive(Clone)]
pub(super) struct Handles {
    pub plans: Arc<DashMap<usize, ChannelPlan>>,
    pub cache: Arc<LruCache>,
    pub metrics: Arc<Metrics>,
    /// Lock-free PDC snapshot subscription. `None` = no PDC wiring.
    pub pdc: Option<Arc<ArcSwap<PdcState>>>,
}

/// Butler-thread-local state. Never shared. Plain data.
pub(super) struct Local {
    pub regions: RegionMap,
    pub captures: HashMap<CaptureId, ActiveCapture>,
    pub buffer_margin: f64,
    pub next_region_id: u64,
    pub interleave_buffer: Vec<(f32, f32)>,
}

impl Local {
    pub(super) fn new(base_chunk_size: usize) -> Self {
        Self {
            regions: RegionMap::new(),
            captures: HashMap::new(),
            buffer_margin: 1.0,
            next_region_id: 0,
            interleave_buffer: Vec::with_capacity(base_chunk_size),
        }
    }

    pub(super) fn mint_region_id(&mut self) -> RegionId {
        self.next_region_id += 1;
        RegionId(self.next_region_id)
    }
}

pub(super) fn handle_command(
    cmd: ButlerCommand,
    shared: &Handles,
    config: &BufferConfig,
    sample_rate: f64,
    local: &mut Local,
) {
    match cmd {
        ButlerCommand::StreamAudioFile {
            channel_index,
            file_path,
            offset_samples,
        } => {
            handle_stream_file(
                channel_index,
                file_path,
                offset_samples,
                shared,
                sample_rate,
                local,
            );
        }
        ButlerCommand::StopStreaming { channel_index } => {
            if let Some(mut plan) = shared.plans.get_mut(&channel_index) {
                plan.stop_streaming();
            }
        }

        ButlerCommand::SetVarispeed {
            channel_index,
            direction,
            speed,
        } => {
            if let Some(plan) = shared.plans.get(&channel_index) {
                plan.rt_state.set_speed(speed);
                plan.rt_state.set_reverse(direction.is_reverse());
            }
        }

        ButlerCommand::RegisterCapture {
            capture_id,
            consumer,
            file_path,
            sample_rate: cap_sample_rate,
            channels,
        } => {
            let writer = open_wav(&file_path, cap_sample_rate, channels);
            local.captures.insert(
                capture_id,
                ActiveCapture {
                    consumer,
                    writer,
                    channels,
                },
            );
        }
        ButlerCommand::RemoveCapture(capture_id) => {
            if let Some(mut cap_state) = local.captures.remove(&capture_id) {
                flush_capture(&mut cap_state, &shared.metrics, usize::MAX);
                if let Some(writer) = cap_state.writer.take() {
                    let _ = writer.finalize();
                }
            }
        }
        ButlerCommand::Flush(capture_id) => {
            if let Some(cap_state) = local.captures.get_mut(&capture_id) {
                flush_capture(cap_state, &shared.metrics, usize::MAX);
            }
        }

        ButlerCommand::Shutdown => {
            flush_all(
                &mut local.captures,
                &shared.metrics,
                config.flush_threshold,
                true,
            );
        }
    }
}

fn handle_stream_file(
    channel_index: usize,
    file_path: PathBuf,
    offset_samples: usize,
    shared: &Handles,
    sample_rate: f64,
    local: &mut Local,
) {
    let wave = if let Some(cached) = shared.cache.get(&file_path) {
        shared.metrics.record_cache_hit();
        cached
    } else {
        shared.metrics.record_cache_miss();
        match Wave::load(&file_path) {
            Ok(w) => {
                let arc_wave = Arc::new(w);
                let bytes = arc_wave.len() as u64 * arc_wave.channels() as u64 * 4;
                shared.metrics.record_read(bytes);
                shared.cache.insert(file_path.clone(), arc_wave.clone());
                arc_wave
            }
            Err(_) => return,
        }
    };

    let file_length = wave.len() as u64;

    let buffer_capacity = buffer_size_for_file(file_length, sample_rate);
    let region_id = local.mint_region_id();

    let (producer, consumer) =
        RegionBuffer::with_capacity(region_id, file_path.clone(), buffer_capacity);

    let pdc_preroll = shared.pdc.as_ref().map_or(0, |pdc| {
        let snap = pdc.load();
        if !snap.is_active() {
            return 0;
        }
        snap.channel_compensations()
            .get(channel_index)
            .copied()
            .unwrap_or(0) as u64
    });

    let adjusted_offset = (offset_samples as u64).saturating_sub(pdc_preroll);
    producer.set_file_position(adjusted_offset);

    local.regions.register(region_id, producer);

    shared.plans.entry(channel_index).or_default();

    let file_sr = wave.sample_rate();
    let src_ratio = if (file_sr - sample_rate).abs() < 0.01 {
        1.0
    } else {
        (file_sr / sample_rate) as f32
    };

    if let Some(mut plan) = shared.plans.get_mut(&channel_index) {
        plan.start_streaming(Arc::new(Mutex::new(consumer)));
        plan.pdc_preroll = pdc_preroll;
        plan.rt_state.set_src_ratio(src_ratio);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mint_region_id_is_monotonic() {
        let mut local = Local::new(4096);

        let id1 = local.mint_region_id();
        let id2 = local.mint_region_id();
        let id3 = local.mint_region_id();

        assert_eq!(id1, RegionId(1));
        assert_eq!(id2, RegionId(2));
        assert_eq!(id3, RegionId(3));
    }
}
