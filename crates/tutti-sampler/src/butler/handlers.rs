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
use super::io::loops::{buffer_size_for_file, capture_samples, fadein_samples, fadeout_samples};
use super::metrics::Metrics;
use super::plan::{ChannelPlan, LoopConfig};
use super::prefetch::RegionBuffer;
use super::region_map::RegionMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum RunState {
    #[default]
    Running,
    Paused,
}

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
    pub run_state: RunState,
    pub buffer_margin: f64,
    pub next_region_id: u64,
    pub interleave_buffer: Vec<(f32, f32)>,
}

impl Local {
    pub(super) fn new(base_chunk_size: usize) -> Self {
        Self {
            regions: RegionMap::new(),
            captures: HashMap::new(),
            run_state: RunState::Running,
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
        ButlerCommand::Run => {
            local.run_state = RunState::Running;
        }
        ButlerCommand::Pause => {
            local.run_state = RunState::Paused;
        }
        ButlerCommand::WaitForCompletion => {
            flush_all(
                &mut local.captures,
                &shared.metrics,
                config.flush_threshold,
                true,
            );
        }

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

        ButlerCommand::SeekStream {
            channel_index,
            position_samples,
        } => {
            handle_seek_stream(channel_index, position_samples, shared, config, local);
        }

        ButlerCommand::SetLoopRange {
            channel_index,
            start_samples,
            end_samples,
            crossfade_samples,
        } => {
            handle_set_loop_range(
                channel_index,
                start_samples,
                end_samples,
                crossfade_samples,
                shared,
                local,
            );
        }

        ButlerCommand::ClearLoopRange { channel_index } => {
            if let Some(mut plan) = shared.plans.get_mut(&channel_index) {
                plan.clear_loop_range();
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

        ButlerCommand::SetBufferMargin { margin } => {
            local.buffer_margin = margin.clamp(0.5, 3.0);
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

fn handle_seek_stream(
    channel_index: usize,
    position_samples: u64,
    shared: &Handles,
    config: &BufferConfig,
    local: &mut Local,
) {
    let Some(plan) = shared.plans.get(&channel_index) else {
        return;
    };
    let Some(region_id) = plan.link.as_ref().map(|l| l.region_id) else {
        return;
    };
    let Some(producer) = local.regions.get_mut(region_id) else {
        return;
    };
    let crossfade_len = config.seek_crossfade_samples;

    let pdc_preroll = plan.pdc_preroll;
    let adjusted_position = position_samples.saturating_sub(pdc_preroll);

    let fadeout = fadeout_samples(&plan, crossfade_len);

    plan.set_seeking(true);

    plan.flush_buffer();
    producer.set_file_position(adjusted_position);

    let fadein = fadein_samples(
        &shared.cache,
        &shared.metrics,
        producer.file_path(),
        adjusted_position,
        crossfade_len,
    );

    if !fadeout.is_empty() && !fadein.is_empty() {
        plan.rt_state.start_seek_crossfade(fadeout, fadein);
    }

    plan.set_seeking(false);
}

fn handle_set_loop_range(
    channel_index: usize,
    start_samples: u64,
    end_samples: u64,
    crossfade_samples: usize,
    shared: &Handles,
    local: &mut Local,
) {
    let Some(mut plan) = shared.plans.get_mut(&channel_index) else {
        return;
    };

    let mut new_cfg = LoopConfig {
        range: (start_samples, end_samples),
        crossfade_samples,
        preloop_buffer: None,
    };

    if let Some(link) = plan.link.as_ref() {
        if let Some(producer) = local.regions.get_mut(link.region_id) {
            if producer.file_position() > end_samples {
                plan.flush_buffer();
                producer.set_file_position(start_samples);
            }

            if crossfade_samples > 0 {
                if let Some(wave) = shared.cache.get(producer.file_path()) {
                    new_cfg.preloop_buffer = Some(capture_samples(
                        &wave,
                        start_samples as usize,
                        crossfade_samples,
                    ));
                }
            }
        }
    }

    plan.set_loop_config(new_cfg);
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
