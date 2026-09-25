//! Butler command handlers + their state types.
//!
//! `Handles` — Arc'd state also held by `ButlerThread`.
//! `Local`    — butler-thread-local data (producers, captures, ...).
//! `handle_command` — dispatch; each arm is either inline or a `handle_*` fn.

use std::path::PathBuf;
use std::sync::Arc;

use dashmap::DashMap;
use tutti_core::RtPublish;
use tutti_core::{AtomicF64, Ordering, SampleRate, Samples, SrcRatio};

use super::cache::LruCache;
use super::command::{ButlerCommand, RegionId};
use super::config::BufferConfig;
use super::io::refill::load_wave;
use super::loops::{buffer_size_for_file, head_position, ring_head_frames, RingLoop};
use super::metrics::Metrics;
use super::plan::{ChannelPlan, LoopConfig};
use super::prefetch::{share_reader, RegionBuffer};
use super::preroll::{reposition_click_free, reposition_from};
use super::region_map::RegionMap;

/// Arc'd handles shared between `ButlerThread` (controller) and the butler
/// loop. `Clone` is cheap — every field is an `Arc` or `Option<Arc>`.
#[derive(Clone)]
pub(super) struct Handles {
    pub plans: Arc<DashMap<usize, ChannelPlan>>,
    pub cache: Arc<LruCache>,
    pub metrics: Arc<Metrics>,
    /// Lock-free subscription to the compensation table. `None` = no PDC wiring.
    pub pdc: Option<Arc<RtPublish<Vec<Samples>>>>,
    /// The session rate each stream's [`SrcRatio`] is derived against.
    pub session_rate: SessionRate,
}

/// The session (graph) rate, one cell shared by the controller, the butler
/// and every [`Status`](crate::Status) snapshot.
///
/// Shared rather than copied into each because a device restart moves it
/// (`DiskStreamer::set_sample_rate`): a copy taken at build time is how a
/// stream kept converting to 44.1 kHz on a 48 kHz device — every streamed clip
/// ~8.8% sharp and fast, with no error anywhere.
#[derive(Clone)]
pub(crate) struct SessionRate(Arc<AtomicF64>);

impl SessionRate {
    pub(crate) fn new(rate: SampleRate) -> Self {
        Self(Arc::new(AtomicF64::new(rate.get())))
    }

    pub(crate) fn get(&self) -> SampleRate {
        SampleRate(self.0.load(Ordering::SeqCst))
    }

    /// Move the rate, then re-derive the ratio of every stream that is open.
    ///
    /// **Why the ratio is derived under the plan's lock on both sides.**
    /// `handle_stream_file` reads the rate and sets the ratio while it holds
    /// the plan's entry; this stores the rate first and then takes each entry.
    /// So a stream the butler opens concurrently either set its ratio before
    /// this took its entry (and is re-derived here), or takes the entry after
    /// this released it, and then reads the new rate. Neither order leaves a
    /// stream on the old ratio.
    pub(super) fn set(&self, rate: SampleRate, plans: &DashMap<usize, ChannelPlan>) {
        self.0.store(rate.get(), Ordering::SeqCst);
        for plan in plans.iter() {
            if let Some(link) = &plan.link {
                plan.rt_state
                    .set_src_ratio(SrcRatio::for_rates(link.file_rate, rate));
            }
        }
    }
}

/// Butler-thread-local state. Never shared. Plain data.
pub(super) struct Local {
    /// Every live region's producer half.
    pub regions: RegionMap,
    /// Headroom multiplier on the refill threshold — a larger margin refills
    /// earlier and more often.
    pub buffer_margin: f64,
    /// Monotonic id source; see [`mint_region_id`](Self::mint_region_id).
    pub next_region_id: u64,
    /// Flat interleaved refill scratch. Butler-thread-local, so it may grow.
    pub interleave_buffer: Vec<f32>,
}

impl Local {
    /// Thread-local state with no regions and a scratch buffer pre-sized for one
    /// `base_chunk_size` refill.
    pub(super) fn new(base_chunk_size: usize) -> Self {
        Self {
            regions: RegionMap::new(),
            buffer_margin: 1.0,
            next_region_id: 0,
            interleave_buffer: Vec::with_capacity(base_chunk_size),
        }
    }

    /// The next region id, starting at 1. Monotonic and never reused, which is
    /// what lets [`RegionMap`] skip compaction on removal.
    pub(super) fn mint_region_id(&mut self) -> RegionId {
        self.next_region_id += 1;
        RegionId(self.next_region_id)
    }
}

/// Dispatch one command on the butler thread.
///
/// Every arm is infallible and silently no-ops when its channel is absent or not
/// streaming: commands race the streams they name (a `Stop` may arrive after the
/// stream already ended), and there is no caller left to report to by the time
/// the butler sees one.
pub(super) fn handle_command(
    cmd: ButlerCommand,
    shared: &Handles,
    config: &BufferConfig,
    local: &mut Local,
) {
    match cmd {
        ButlerCommand::StreamAudioFile {
            channel_index,
            file_path,
            offset_samples,
        } => {
            handle_stream_file(channel_index, file_path, offset_samples, shared, local);
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
            crossfade_frames,
        } => {
            handle_set_stream_loop(
                channel_index,
                Some((range, crossfade_frames)),
                shared,
                config,
                local,
            );
        }
        ButlerCommand::ClearStreamLoop { channel_index } => {
            handle_set_stream_loop(channel_index, None, shared, config, local);
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
                plan.rt_state.set_direction(direction);
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
///
/// **The verdict comes from [`crate::probe`], not from a local copy of the
/// rule.** A host picks a playback tier before any of this runs, and it must
/// reach the same conclusion this function does — so "can the butler stream it"
/// is asked in exactly one place. Re-deriving it here is how the two would drift
/// the next time the conditions changed.
#[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
fn open_stream(file_path: &std::path::Path) -> Option<(tutti_io::WaveMetadata, tutti_io::FileIn)> {
    if !crate::probe(file_path).ok()?.streamable {
        return None;
    }
    // Streamable, so both of these succeed — but they are still fallible calls
    // and are handled rather than unwrapped: the probe read the header a moment
    // ago, and a file can be replaced between the two reads.
    let meta = tutti_io::Wave::probe_metadata(file_path).ok()?;
    let decoder = tutti_io::FileIn::open(file_path).ok()?;
    Some((meta, decoder))
}

/// Start streaming `file_path` on `channel_index` from frame `offset_samples`.
///
/// Probes metadata for ring sizing and the conversion ratio *without* decoding
/// the whole file, builds the region ring at the **file's own width**, installs
/// the reader on the channel's plan, and pins the wave in the cache for the
/// stream's lifetime. Falls back to the whole-file `load_wave` path when the
/// format is not seekable.
///
/// Silently returns when the file cannot be turned into audio at all — there is
/// no caller left to report to on this thread.
fn handle_stream_file(
    channel_index: usize,
    file_path: PathBuf,
    offset_samples: usize,
    shared: &Handles,
    local: &mut Local,
) {
    // Probe metadata (frame count / sample rate) for ring sizing + src_ratio
    // WITHOUT decoding the whole file. Prefer real incremental streaming; fall
    // back to the whole-file `load_wave` + `LruCache` path when the format
    // isn't seekable (no frame count) or opening the stream decoder fails.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    let (file_length, file_sr, file_channels, decoder) = match open_stream(&file_path) {
        Some((meta, decoder)) => (
            meta.total_frames.unwrap_or(0),
            // Decoder metadata carries the header's integer rate.
            SampleRate::from(meta.sample_rate),
            decoder.channels(),
            Some(decoder),
        ),
        None => {
            let Some(wave) = load_wave(&shared.cache, &shared.metrics, &file_path) else {
                return;
            };
            (wave.len() as u64, wave.sample_rate(), wave.channels(), None)
        }
    };
    #[cfg(not(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")))]
    let (file_length, file_sr, file_channels) = {
        let Some(wave) = load_wave(&shared.cache, &shared.metrics, &file_path) else {
            return;
        };
        (wave.len() as u64, wave.sample_rate(), wave.channels())
    };

    // Sizing only: a rate that moves before the ratio is set below changes the
    // ring's depth, never what it plays.
    let buffer_capacity = buffer_size_for_file(file_length, shared.session_rate.get());
    let region_id = local.mint_region_id();

    // The ring carries the file at its OWN width: the streaming tier reads it
    // back through the same channel policy the in-memory tier uses, so folding
    // here would discard channels before that policy ever sees them.
    // `mut` is used by the `set_decoder` call below, which every codec feature
    // gates — so a build with none of them on sees an unused `mut` rather than
    // a dead binding.
    #[cfg_attr(
        not(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")),
        allow(unused_mut)
    )]
    let (mut producer, consumer) =
        RegionBuffer::with_capacity(region_id, file_path.clone(), buffer_capacity, file_channels);

    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    if let Some(decoder) = decoder {
        producer.set_decoder(decoder);
    }

    let pdc_preroll = shared.pdc.as_ref().map_or(0, |pdc| {
        pdc.read()
            .get(channel_index)
            .copied()
            .unwrap_or_default()
            .get() as u64
    });

    let adjusted_offset = (offset_samples as u64).saturating_sub(pdc_preroll);
    producer.set_file_position(adjusted_offset);

    local.regions.register(region_id, producer);

    shared.plans.entry(channel_index).or_default();

    // Pin the streamed wave in the LRU cache for the stream's lifetime. On the
    // fallback path `load_wave` inserted the whole file into the cache, so this
    // protects that resident Wave from being evicted mid-read (a fully-buffered
    // stream goes cold and would otherwise be picked as the LRU victim). On the
    // incremental-decode path the wave isn't cached, so `pin` finds no entry and
    // the guard is inert — harmless. Held inside the `Link` (see
    // `ChannelPlan::start_streaming`) so it releases when streaming stops.
    let cache_pin = Some(shared.cache.pin(&file_path));

    if let Some(mut plan) = shared.plans.get_mut(&channel_index) {
        plan.start_streaming(
            share_reader(consumer),
            cache_pin,
            file_sr,
            file_length,
            file_path,
            Arc::downgrade(&shared.cache),
        );
        plan.pdc_preroll = pdc_preroll;
        // Same derivation the in-memory tier uses
        // (`MemorySource::set_session_sample_rate`), through the one shared
        // constructor. Hand-rolling the division here instead would leave the
        // two tiers agreeing only by convention. The rate is read while this
        // holds the plan, which is what `SessionRate::set` relies on.
        plan.rt_state
            .set_src_ratio(SrcRatio::for_rates(file_sr, shared.session_rate.get()));
    }
}

/// Set (`Some((range, crossfade_frames))`) or clear (`None`) a streaming
/// channel's loop, and reposition the stream so the change is heard at once.
///
/// The ring holds what the refill wrote under the old loop, as much as 30 s
/// of it (`buffer_size_for_file`), so a change left to the refill would be
/// heard only once that drained. Instead the change takes effect **at the
/// butler's next cycle, at the frame the audio thread reads next**: the ring
/// is flushed at its head ([`head_position`]) and refilled from that same
/// straight position under the new loop — where the memory tier, whose loop
/// change is a store, plays the same position. It moves as a seek does, through
/// the seek crossfade (the fadeout captured under the old loop, the fadein under
/// the new). A reverse stream ignores the loop, so its change is only stored.
///
/// The loop's span is taken on the file's length, and a crossfaded loop's
/// lead-in is captured here, once, off the audio thread (`RingLoop::capture`),
/// so no refill re-reads it.
///
/// No-op when the channel isn't currently streaming (no `link`) — the loop
/// config has nowhere to live without an active stream.
fn handle_set_stream_loop(
    channel_index: usize,
    setting: Option<((u64, u64), usize)>,
    shared: &Handles,
    config: &BufferConfig,
    local: &mut Local,
) {
    let Some(mut plan) = shared.plans.get_mut(&channel_index) else {
        return;
    };
    let Some(region_id) = plan.link.as_ref().map(|link| link.region_id) else {
        return;
    };
    let Some(writer) = local.regions.get_mut(region_id) else {
        return;
    };

    let loop_config = setting.map(|(range, crossfade_frames)| {
        let file_frames = plan.link.as_ref().map_or(0, |link| link.file_frames) as usize;
        // The whole file only for a fade's lead-in; a hard loop reads nothing.
        let wave = (crossfade_frames > 0)
            .then(|| load_wave(&shared.cache, &shared.metrics, writer.file_path()))
            .flatten();
        LoopConfig {
            range,
            crossfade_frames,
            // Width from the ring, not the wave: the ring's stride is what the
            // refill blends the lead-in at.
            ring: RingLoop::capture(
                range,
                crossfade_frames,
                file_frames,
                wave.as_deref(),
                writer.channels(),
            ),
        }
    });

    if plan.rt_state.is_reverse() {
        if let Some(link) = plan.link.as_mut() {
            link.set_loop(loop_config);
        }
        return;
    }

    let fadeout = ring_head_frames(
        &plan,
        writer,
        &shared.cache,
        &shared.metrics,
        config.seek_crossfade_frames,
    );
    let head = head_position(writer);
    if let Some(link) = plan.link.as_mut() {
        link.set_loop(loop_config);
    }
    reposition_from(
        &plan,
        writer,
        head,
        fadeout,
        &shared.cache,
        &shared.metrics,
        config,
    );
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

    reposition_click_free(
        &plan,
        writer,
        new_pos,
        &shared.cache,
        &shared.metrics,
        config,
    );
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
