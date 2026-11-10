//! Streamer command handlers + their state types.
//!
//! `Handles` — Arc'd state also held by `StreamerThread`.
//! `Local`    — streamer-thread-local data (producers, captures, ...).
//! `handle_command` — dispatch; each arm is either inline or a `handle_*` fn.

use std::path::PathBuf;
use std::sync::Arc;

use atomic_float::AtomicF64;
use dashmap::DashMap;
use std::sync::atomic::Ordering;
use tutti_types::{SampleRate, SrcRatio};

use super::cache::LruCache;
use super::command::{Reply, StreamId, StreamerCommand};
use super::config::BufferConfig;
use super::control::StreamOrigin;
use super::io::refill::load_wave;
use super::loops::{LeadIn, Mapping, RingLoop, apply_mapping, buffer_size_for_file};
use super::metrics::Metrics;
use super::open::{OpenError, Opened};
use super::plan::{ChannelPlan, LoopConfig};
use super::prefetch::RegionBuffer;
use super::region_map::RegionMap;
use crate::nonempty;
use crate::voice::interp::Place;
use crate::voice_width;
use tutti_types::ChannelLayout;

/// Arc'd handles shared between `StreamerThread` (controller) and the streamer
/// loop. `Clone` is cheap — every field is an `Arc`.
#[derive(Clone)]
pub(super) struct Handles {
    pub plans: Arc<DashMap<StreamId, ChannelPlan>>,
    pub cache: Arc<LruCache>,
    pub metrics: Arc<Metrics>,
    /// The session rate each stream's [`SrcRatio`] is derived against.
    pub session_rate: SessionRate,
}

/// The session (graph) rate, one cell shared by the controller, the streamer
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
    /// Why the ratio is derived under the plan's lock on both sides.
    /// `open_stream_region` reads the rate and sets the ratio while it holds
    /// the plan's entry; this stores the rate first and then takes each entry.
    /// So a stream the streamer opens concurrently either set its ratio before
    /// this took its entry (and is re-derived here), or takes the entry after
    /// this released it, and then reads the new rate. Neither order leaves a
    /// stream on the old ratio.
    pub(super) fn set(&self, rate: SampleRate, plans: &DashMap<StreamId, ChannelPlan>) {
        self.0.store(rate.get(), Ordering::SeqCst);
        for plan in plans.iter() {
            if let Some(link) = &plan.link {
                plan.rt_state
                    .set_src_ratio(SrcRatio::for_rates(link.file_rate, rate));
            }
        }
    }
}

/// Streamer-thread-local state. Never shared. Plain data.
pub(super) struct Local {
    /// Every live region's producer half.
    pub regions: RegionMap,
    /// Headroom multiplier on the refill threshold — a larger margin refills
    /// earlier and more often.
    pub buffer_margin: f64,
    /// Flat interleaved refill scratch. Streamer-thread-local, so it may grow.
    pub interleave_buffer: Vec<f32>,
}

impl Local {
    /// Thread-local state with no regions, a scratch buffer pre-sized for one
    /// `base_chunk_size` refill.
    pub(super) fn new(base_chunk_size: usize) -> Self {
        Self {
            regions: RegionMap::new(),
            buffer_margin: 1.0,
            interleave_buffer: Vec::with_capacity(base_chunk_size),
        }
    }
}

/// Dispatch one command on the streamer thread.
///
/// An open is answered on its own reply. Every other arm is infallible and
/// silently no-ops for a stream the streamer no longer holds: commands race
/// the streams they name (a `Stop` may arrive after the stream already ended),
/// and there is no caller left to report to by the time the streamer sees one.
pub(super) fn handle_command(
    cmd: StreamerCommand,
    shared: &Handles,
    config: &BufferConfig,
    local: &mut Local,
) {
    match cmd {
        StreamerCommand::Open {
            id,
            file_path,
            start,
            width,
            reply,
        } => handle_open(id, file_path, start, width, reply, shared, config, local),
        StreamerCommand::StopStreaming { stream } => close_stream(stream, shared, local),

        StreamerCommand::SetStreamLoop {
            stream,
            range,
            crossfade_frames,
        } => {
            handle_set_stream_loop(
                stream,
                Some((range, crossfade_frames)),
                shared,
                config,
                local,
            );
        }
        StreamerCommand::ClearStreamLoop { stream } => {
            handle_set_stream_loop(stream, None, shared, config, local);
        }

        StreamerCommand::Shutdown => {}
    }
}

/// Close `stream`: its plan (and with it the link, the cache pin and the
/// stream's record) and its region go together, or every stopped stream would
/// leak its ring and decoder for the streamer's lifetime.
fn close_stream(stream: StreamId, shared: &Handles, local: &mut Local) {
    shared.plans.remove(&stream);
    local.regions.remove(stream);
}

/// Try to open an incremental disk decoder for `file_path`. Returns
/// `Some((metadata, decoder))` when the format is seekable (has a frame count
/// and the decoder opened), so the region can stream real ranges from disk.
/// Returns `None` — signalling the whole-file `load_wave` fallback — when a
/// codec feature isn't compiled in, the probe fails, or the format is
/// non-seekable.
///
/// The verdict comes from [`crate::probe()`], not from a local copy of the
/// rule. A host picks a playback tier before any of this runs, and it must
/// reach the same conclusion this function does — so "can the streamer stream it"
/// is asked in exactly one place. Re-deriving it here is how the two would drift
/// the next time the conditions changed.
#[cfg(codec)]
fn open_stream(
    file_path: &std::path::Path,
) -> Option<(tutti_codec::WaveMetadata, tutti_codec::FileIn)> {
    if !crate::probe(file_path).ok()?.streamable {
        return None;
    }
    // Streamable, so both of these succeed — but they are still fallible calls
    // and are handled rather than unwrapped: the probe read the header a moment
    // ago, and a file can be replaced between the two reads.
    let meta = tutti_codec::Wave::read_metadata(file_path).ok()?;
    let decoder = tutti_codec::FileIn::open(file_path).ok()?;
    Some((meta, decoder))
}

/// Open stream `id` on `file_path`, its window starting at frame `start`, and
/// answer on `reply`: the reader's parts, or why there is none.
///
/// A caller that dropped its `Opening` before the answer abandoned the open,
/// so the stream is closed again rather than fed for nobody.
#[allow(clippy::too_many_arguments)]
fn handle_open(
    id: StreamId,
    file_path: PathBuf,
    start: u64,
    width: Option<ChannelLayout>,
    reply: Reply,
    shared: &Handles,
    config: &BufferConfig,
    local: &mut Local,
) {
    let answer = open_stream_region(id, file_path, start, width, shared, config, local);
    if let Err(abandoned) = reply.send(answer)
        && abandoned.0.is_ok()
    {
        close_stream(id, shared, local);
    }
}

/// Why `file_path` could not be turned into audio: the file itself when it
/// cannot be opened, else a format this build cannot decode.
fn unreadable(file_path: &std::path::Path) -> OpenError {
    match std::fs::File::open(file_path) {
        Err(error) => OpenError::Io(error.kind()),
        Ok(_) => OpenError::Format,
    }
}

/// Start streaming `file_path` as stream `id` from frame `start`.
///
/// Probes metadata for ring sizing and the conversion ratio *without* decoding
/// the whole file, builds the region ring at the file's own width, installs
/// the stream's plan, and pins the wave in the cache for the stream's
/// lifetime. Falls back to the whole-file `load_wave` path when the format is
/// not seekable. The placement into `width` (the file's own when `None`) is
/// built, and so refused, before anything is built.
fn open_stream_region(
    id: StreamId,
    file_path: PathBuf,
    start: u64,
    width: Option<ChannelLayout>,
    shared: &Handles,
    config: &BufferConfig,
    local: &mut Local,
) -> Result<Opened, OpenError> {
    // Probe metadata (frame count / sample rate) for ring sizing + src_ratio
    // WITHOUT decoding the whole file. Prefer real incremental streaming; fall
    // back to the whole-file `load_wave` + `LruCache` path when the format
    // isn't seekable (no frame count) or opening the stream decoder fails.
    #[cfg(codec)]
    let (file_length, file_sr, file_topology, decoder, resident) = match open_stream(&file_path) {
        Some((meta, decoder)) => (
            meta.total_frames.unwrap_or(0),
            // Decoder metadata carries the header's integer rate.
            SampleRate::from(meta.sample_rate),
            decoder.topology().clone(),
            Some(decoder),
            None,
        ),
        None => {
            let wave = load_wave(&shared.cache, &shared.metrics, &file_path)
                .ok_or_else(|| unreadable(&file_path))?;
            (
                wave.len() as u64,
                wave.sample_rate(),
                wave.topology().clone(),
                None,
                Some(wave),
            )
        }
    };
    #[cfg(not(codec))]
    let (file_length, file_sr, file_topology, resident) = {
        let wave = load_wave(&shared.cache, &shared.metrics, &file_path)
            .ok_or_else(|| unreadable(&file_path))?;
        (
            wave.len() as u64,
            wave.sample_rate(),
            wave.topology().clone(),
            Some(wave),
        )
    };

    // Where the file's channels go in the voice, built (and so refused) now,
    // before a ring exists to free again.
    let width = voice_width(width.unwrap_or_else(|| nonempty(file_topology.layout())));
    let place = Place::for_topology(&file_topology, width).map_err(OpenError::Placement)?;

    // Sizing only: a rate that moves before the ratio is set below changes the
    // ring's depth, never what it plays.
    let buffer_capacity = buffer_size_for_file(file_length, shared.session_rate.get());

    // The ring carries the file at its OWN width and order, and its topology:
    // the voice that reads the ring places it through the same `Place` the
    // in-memory tier uses, so folding or permuting here would only be a second
    // copy of that placement (and the loop's crossfades and an
    // offline fork's own read of the file all work at the file's width).
    // `mut` is used by the `set_decoder` call below, which every codec feature
    // gates — so a build with none of them on sees an unused `mut` rather than
    // a dead binding.
    let (mut producer, ring, pos) = RegionBuffer::for_file(
        id,
        file_path.clone(),
        buffer_capacity,
        file_topology,
        file_length as usize,
    );

    #[cfg(codec)]
    if let Some(decoder) = decoder {
        producer.set_decoder(decoder);
    }
    // A file that cannot seek is held whole by its writer, not only by the
    // cache: a file too large for the cache still streams.
    if let Some(wave) = resident {
        producer.set_resident(wave);
    }

    // The window starts where the reader will play: the offset. A placed
    // voice reads at its clock; the compiler aligns the voice in the graph.
    RegionBuffer::place(&mut producer, start, config.seek_crossfade_frames);
    local.regions.register(id, producer);

    // Pin the streamed wave in the LRU cache for the stream's lifetime. On the
    // fallback path `load_wave` inserted the whole file into the cache, so this
    // protects that resident Wave from being evicted mid-read (a fully-buffered
    // stream goes cold and would otherwise be picked as the LRU victim). On the
    // incremental-decode path the wave isn't cached, so `pin` finds no entry and
    // the guard is inert — harmless. Held inside the `Link` (see
    // `ChannelPlan::start_streaming`) so it releases when the stream closes.
    let cache_pin = Some(shared.cache.pin(&file_path));

    // The plan is installed and its ratio set while this holds the entry,
    // which is what `SessionRate::set` relies on: it stores the rate first and
    // then takes each entry, so a stream is either re-derived by it, or
    // derives against the new rate itself.
    let mut plan = shared.plans.entry(id).or_default();
    plan.start_streaming(
        id,
        cache_pin,
        file_sr,
        file_length,
        file_path,
        Arc::downgrade(&shared.cache),
    );
    // Same derivation the in-memory tier uses
    // (`MemorySource::set_session_sample_rate`), through the one shared
    // constructor. Hand-rolling the division here instead would leave the
    // two tiers agreeing only by convention.
    plan.rt_state
        .set_src_ratio(SrcRatio::for_rates(file_sr, shared.session_rate.get()));
    let origin = plan
        .link
        .as_ref()
        .map(|link| StreamOrigin::new(Arc::clone(&link.record)))
        .expect("the link was installed just above");
    let rt_state = plan.rt_state();
    drop(plan);

    Ok(Opened {
        id,
        ring,
        pos,
        rt_state,
        file_rate: file_sr,
        origin,
        place,
        width,
    })
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
/// No-op when `stream` isn't open (no plan, no `link`) — the loop config has
/// nowhere to live without an active stream, and the loop was set on a voice
/// whose stream has since stopped.
fn handle_set_stream_loop(
    stream: StreamId,
    setting: Option<((u64, u64), usize)>,
    shared: &Handles,
    config: &BufferConfig,
    local: &mut Local,
) {
    let Some((len, rate)) = shared.plans.get(&stream).and_then(|plan| {
        let link = plan.link.as_ref()?;
        Some((link.file_frames as usize, plan.rt_state.read_rate().get()))
    }) else {
        return;
    };
    let Some(writer) = local.regions.get_mut(stream) else {
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

    if let Some(mut plan) = shared.plans.get_mut(&stream)
        && let Some(link) = plan.link.as_mut()
    {
        link.set_loop(recorded.map(|(range, crossfade_frames)| LoopConfig {
            range,
            crossfade_frames,
        }));
    }
}
