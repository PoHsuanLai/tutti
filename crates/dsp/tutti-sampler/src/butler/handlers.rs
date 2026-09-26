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
use super::loops::{apply_mapping, buffer_size_for_file, LeadIn, Mapping, RingLoop, RingMap};
use super::metrics::Metrics;
use super::plan::{ChannelPlan, LoopConfig};
use super::prefetch::RegionBuffer;
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
            handle_stream_file(
                channel_index,
                file_path,
                offset_samples,
                shared,
                config,
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
            handle_seek_stream(channel_index, file_position, shared);
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
    config: &BufferConfig,
    local: &mut Local,
) {
    // Probe metadata (frame count / sample rate) for ring sizing + src_ratio
    // WITHOUT decoding the whole file. Prefer real incremental streaming; fall
    // back to the whole-file `load_wave` + `LruCache` path when the format
    // isn't seekable (no frame count) or opening the stream decoder fails.
    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    let (file_length, file_sr, file_channels, decoder, resident) = match open_stream(&file_path) {
        Some((meta, decoder)) => (
            meta.total_frames.unwrap_or(0),
            // Decoder metadata carries the header's integer rate.
            SampleRate::from(meta.sample_rate),
            decoder.channels(),
            Some(decoder),
            None,
        ),
        None => {
            let Some(wave) = load_wave(&shared.cache, &shared.metrics, &file_path) else {
                return;
            };
            (
                wave.len() as u64,
                wave.sample_rate(),
                wave.channels(),
                None,
                Some(wave),
            )
        }
    };
    #[cfg(not(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg")))]
    let (file_length, file_sr, file_channels, resident) = {
        let Some(wave) = load_wave(&shared.cache, &shared.metrics, &file_path) else {
            return;
        };
        (
            wave.len() as u64,
            wave.sample_rate(),
            wave.channels(),
            Some(wave),
        )
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
    let (mut producer, consumer) = RegionBuffer::for_file(
        region_id,
        file_path.clone(),
        buffer_capacity,
        file_channels,
        file_length as usize,
    );

    #[cfg(any(feature = "wav", feature = "flac", feature = "mp3", feature = "ogg"))]
    if let Some(decoder) = decoder {
        producer.set_decoder(decoder);
    }
    // A file that cannot seek is held whole by its writer, not only by the
    // cache: a file too large for the cache still streams.
    if let Some(wave) = resident {
        producer.set_resident(wave);
    }

    let pdc_preroll = shared.pdc.as_ref().map_or(0, |pdc| {
        pdc.read()
            .get(channel_index)
            .copied()
            .unwrap_or_default()
            .get() as u64
    });

    // The window starts where the reader will play: the offset, pre-rolled.
    // A free-running reader starts at the offset; a placed voice at its clock.
    let start = (offset_samples as u64).saturating_sub(pdc_preroll);
    RegionBuffer::place(
        &consumer,
        start,
        offset_samples as u64,
        pdc_preroll,
        config.seek_crossfade_frames,
    );

    shared.plans.entry(channel_index).or_default();
    // A channel left reversed streams its new file reversed.
    let reverse = shared
        .plans
        .get(&channel_index)
        .is_some_and(|plan| plan.rt_state.is_reverse());
    if reverse {
        let mapping = Mapping {
            reverse: true,
            ..Mapping::plain(file_length as usize)
        };
        consumer.publish_map(RingMap::plain(mapping.arrangement(), 0));
        producer.set_content(super::loops::Content::new(mapping));
    }

    local.regions.register(region_id, producer);

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
            consumer,
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
/// channel's loop.
///
/// The new mapping goes through [`apply_mapping`]: free when it changes no
/// frame the ring holds, else a switch the reader crosses (see `loops`'
/// module docs) — heard where the memory tier hears it when that is far
/// enough ahead, else just past the block the reader is in, crossfaded. A
/// reversed stream ignores its loop, so there the change is only stored.
///
/// Only the frames the loop needs are read — its fade's lead-in, and its body
/// when it is short (`RingLoop::capture`), through the region's decoder — and
/// the plan's lock is released before any read: a loop change never decodes
/// the whole file, and never holds the plan map while it reads. A lead-in that
/// cannot be read plays the loop hard, is logged, and is what the stream's
/// record says (so a fork plays it hard too).
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
    let Some((region_id, len, rate)) = shared.plans.get(&channel_index).and_then(|plan| {
        let link = plan.link.as_ref()?;
        Some((
            link.region_id,
            link.file_frames as usize,
            plan.rt_state.read_rate().get(),
        ))
    }) else {
        return;
    };
    let Some(writer) = local.regions.get_mut(region_id) else {
        return;
    };

    let channels = writer.channels();
    let mut recorded = setting;
    let ring_loop = setting.and_then(|(range, crossfade_frames)| {
        let (ring_loop, lead_in) =
            RingLoop::capture(range, crossfade_frames, len, channels, &mut |at, out| {
                writer.read_file(at, out)
            })?;
        if lead_in == LeadIn::Unreadable {
            tracing::warn!(
                "could not read the crossfade lead-in of loop {range:?} in {}; \
                 the loop plays hard",
                writer.file_path().display()
            );
            recorded = Some((range, 0));
        }
        Some(ring_loop)
    });
    let new = Mapping {
        ring_loop,
        ..writer.content().current.clone()
    };
    apply_mapping(writer, new, config.seek_crossfade_frames, rate);

    if let Some(mut plan) = shared.plans.get_mut(&channel_index) {
        if let Some(link) = plan.link.as_mut() {
            link.set_loop(recorded.map(|(range, crossfade_frames)| LoopConfig {
                range,
                crossfade_frames,
            }));
        }
    }
}

/// Seek a stream's free-running reader to absolute file frame
/// `file_position`, and move the butler's window there (less the channel's
/// PDC preroll) so it is filled before the reader asks.
///
/// Free-running readers only. A placed voice follows its clock (seek the
/// clock), and it is the one that says where it plays: moving the window for
/// it here would be a second writer of that position, dragging the window away
/// from the voice until its next block said otherwise. So on a stream whose
/// live reader is placed, the request is relayed and the window left alone.
///
/// No-op when the channel isn't streaming.
pub(super) fn handle_seek_stream(channel_index: usize, file_position: u64, shared: &Handles) {
    let Some(plan) = shared.plans.get(&channel_index) else {
        return;
    };
    let Some(link) = plan.link.as_ref() else {
        return;
    };
    link.consumer.request_seek(file_position);
    if !link.consumer.placed() {
        link.consumer
            .set_play(file_position.saturating_sub(plan.pdc_preroll));
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
