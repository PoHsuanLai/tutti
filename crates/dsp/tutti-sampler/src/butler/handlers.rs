//! Butler command handlers + their state types.
//!
//! `Handles` — Arc'd state also held by `ButlerThread`.
//! `Local`    — butler-thread-local data (producers, captures, ...).
//! `handle_command` — dispatch; each arm is either inline or a `handle_*` fn.

use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use dashmap::DashMap;
use tutti_core::PdcState;

use super::cache::LruCache;
use super::command::{ButlerCommand, RegionId};
use super::config::BufferConfig;
use super::io::loops::{buffer_size_for_file, capture_samples, fadein_samples, fadeout_samples};
use super::io::refill::load_wave;
use super::metrics::Metrics;
use super::plan::{ChannelPlan, LoopConfig};
use super::prefetch::{share_reader, RegionBuffer};
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
    pub buffer_margin: f64,
    pub next_region_id: u64,
    pub interleave_buffer: Vec<[f32; 2]>,
}

impl Local {
    pub(super) fn new(base_chunk_size: usize) -> Self {
        Self {
            regions: RegionMap::new(),
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
                // Capture the region id before dropping the link so its writer
                // (ring buffer + RegionMeta) can be freed from the RegionMap;
                // otherwise every stopped stream leaks for the butler's lifetime.
                let region_id = plan.link.as_ref().map(|link| link.region_id);
                plan.stop_streaming();
                drop(plan);
                if let Some(region_id) = region_id {
                    local.regions.remove(region_id);
                }
            }
        }

        ButlerCommand::SetStreamLoop {
            channel_index,
            range,
            crossfade_samples,
        } => {
            handle_set_stream_loop(channel_index, range, crossfade_samples, shared, local);
        }
        ButlerCommand::ClearStreamLoop { channel_index } => {
            if let Some(mut plan) = shared.plans.get_mut(&channel_index) {
                if let Some(link) = plan.link.as_mut() {
                    link.loop_config = None;
                }
            }
        }

        ButlerCommand::SeekStream {
            channel_index,
            file_position,
        } => {
            handle_seek_stream(channel_index, file_position, shared, config, local);
        }

        ButlerCommand::SetVarispeed {
            channel_index,
            direction,
            speed,
        } => {
            if let Some(plan) = shared.plans.get(&channel_index) {
                plan.rt_state.set_speed(speed);
                plan.rt_state
                    .set_direction(crate::Direction::from_reverse(direction.is_reverse()));
            }
        }

        ButlerCommand::Shutdown => {}
    }
}

/// Try to open an incremental disk decoder for `file_path`. Returns
/// `Some((metadata, decoder))` when the format is seekable (has a frame count
/// and the decoder opened), so the region can stream real ranges from disk.
/// Returns `None` — signalling the whole-file `load_wave` fallback — when a
/// codec feature isn't compiled in, the probe fails, or the format is
/// non-seekable.
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
fn open_stream(
    file_path: &std::path::Path,
) -> Option<(tutti_core::WaveMetadata, tutti_core::FileIn)> {
    let meta = tutti_core::Wave::probe_metadata(file_path).ok()?;
    // Non-seekable formats (no reported frame count) fall back to whole-file.
    meta.total_frames?;
    let decoder = tutti_core::FileIn::open(file_path, None).ok()?;
    // Guard against a decoder that reports itself non-seekable despite a
    // frame count (defensive; open() only sets seekable when n_frames exists).
    if !decoder.seekable() {
        return None;
    }
    Some((meta, decoder))
}

fn handle_stream_file(
    channel_index: usize,
    file_path: PathBuf,
    offset_samples: usize,
    shared: &Handles,
    sample_rate: f64,
    local: &mut Local,
) {
    // Probe metadata (frame count / sample rate) for ring sizing + src_ratio
    // WITHOUT decoding the whole file. Prefer real incremental streaming; fall
    // back to the whole-file `load_wave` + `LruCache` path when the format
    // isn't seekable (no frame count) or opening the stream decoder fails.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    let (file_length, file_sr, decoder) = match open_stream(&file_path) {
        Some((meta, decoder)) => (
            meta.total_frames.unwrap_or(0),
            meta.sample_rate as f64,
            Some(decoder),
        ),
        None => {
            let Some(wave) = load_wave(&shared.cache, &shared.metrics, &file_path) else {
                return;
            };
            (wave.len() as u64, wave.sample_rate(), None)
        }
    };
    #[cfg(not(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")))]
    let (file_length, file_sr) = {
        let Some(wave) = load_wave(&shared.cache, &shared.metrics, &file_path) else {
            return;
        };
        (wave.len() as u64, wave.sample_rate())
    };

    let buffer_capacity = buffer_size_for_file(file_length, sample_rate);
    let region_id = local.mint_region_id();

    let (mut producer, consumer) =
        RegionBuffer::with_capacity(region_id, file_path.clone(), buffer_capacity);

    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    if let Some(decoder) = decoder {
        producer.set_decoder(decoder);
    }

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

    let src_ratio = if (file_sr - sample_rate).abs() < 0.01 {
        1.0
    } else {
        (file_sr / sample_rate) as f32
    };

    // Pin the streamed wave in the LRU cache for the stream's lifetime. On the
    // fallback path `load_wave` inserted the whole file into the cache, so this
    // protects that resident Wave from being evicted mid-read (a fully-buffered
    // stream goes cold and would otherwise be picked as the LRU victim). On the
    // incremental-decode path the wave isn't cached, so `pin` finds no entry and
    // the guard is inert — harmless. Held inside the `Link` (see
    // `ChannelPlan::start_streaming`) so it releases when streaming stops.
    let cache_pin = Some(shared.cache.pin(&file_path));

    if let Some(mut plan) = shared.plans.get_mut(&channel_index) {
        plan.start_streaming(share_reader(consumer), cache_pin);
        plan.pdc_preroll = pdc_preroll;
        plan.rt_state.set_src_ratio(src_ratio);
    }
}

/// Populate a streaming channel's `link.loop_config`, so `handle_loops`
/// wraps the disk decoder at the loop bounds and `refill_forward` respects
/// them. When a crossfade is requested, the fadein head of the loop is
/// captured once here (off the audio thread) into `preloop_buffer`, so the
/// per-loop crossfade in `handle_loops` doesn't re-read it every wrap.
///
/// No-op when the channel isn't currently streaming (no `link`) — the loop
/// config has nowhere to live without an active stream.
fn handle_set_stream_loop(
    channel_index: usize,
    range: (u64, u64),
    crossfade_samples: usize,
    shared: &Handles,
    local: &mut Local,
) {
    let Some(mut plan) = shared.plans.get_mut(&channel_index) else {
        return;
    };
    let Some(link) = plan.link.as_mut() else {
        return;
    };

    // Capture the loop-start fadein head once, off the audio thread, so the
    // per-wrap crossfade in `handle_loops` never re-reads the file.
    let preloop_buffer = if crossfade_samples > 0 {
        local
            .regions
            .get(link.region_id)
            .and_then(|writer| load_wave(&shared.cache, &shared.metrics, writer.file_path()))
            .map(|wave| capture_samples(&wave, range.0 as usize, crossfade_samples))
    } else {
        None
    };

    link.loop_config = Some(LoopConfig {
        range,
        crossfade_samples,
        preloop_buffer,
    });
}

/// Reposition a live stream to an absolute file sample offset (timeline seek),
/// click-free. Mirrors the PDC reposition
/// ([`apply_pdc_updates`](super::io::pdc::apply_pdc_updates)) but with an
/// explicit target instead of a preroll delta: capture the fadeout tail before
/// moving, flush the ring, seek the writer, capture the fadein head at the new
/// position, and hand both to the audio thread's seek crossfader. `pdc_preroll`
/// is applied to the target (a larger preroll seeks earlier) but not mutated.
///
/// No-op when the channel isn't streaming (no `link`) or its region writer is
/// gone.
pub(super) fn handle_seek_stream(
    channel_index: usize,
    file_position: u64,
    shared: &Handles,
    config: &BufferConfig,
    local: &mut Local,
) {
    let Some(plan) = shared.plans.get(&channel_index) else {
        return;
    };
    let Some(link) = plan.link.as_ref() else {
        return;
    };
    let Some(writer) = local.regions.get_mut(link.region_id) else {
        return;
    };

    let new_pos = file_position.saturating_sub(plan.pdc_preroll);

    let crossfade_len = config.seek_crossfade_samples;
    let fadeout = fadeout_samples(
        &plan,
        &shared.cache,
        &shared.metrics,
        writer.file_path(),
        crossfade_len,
    );

    plan.set_seeking(true);
    plan.flush_buffer();
    writer.set_file_position(new_pos);

    let fadein = fadein_samples(
        &shared.cache,
        &shared.metrics,
        writer.file_path(),
        new_pos,
        crossfade_len,
    );

    if !fadeout.is_empty() && !fadein.is_empty() {
        plan.rt_state.start_seek_crossfade(fadeout, fadein);
    }

    plan.set_seeking(false);
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
