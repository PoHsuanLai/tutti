//! Streamer command handlers + their state types.
//!
//! `Handles` — Arc'd state also held by the driver.
//! `Local`    — streamer-thread-local data (the stream table, scratch).
//! `handle_command` — the driver of [`step`]: it does the file work a command
//! needs, runs the pure step over the table, and performs the effects.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, Weak};

use atomic_float::AtomicF64;
use std::sync::atomic::Ordering;
use tutti_types::{SampleRate, SrcRatio};

use super::cache::LruCache;
use super::command::{Reply, StreamId, StreamerCommand};
use super::config::BufferConfig;
use super::control::{StreamOrigin, StreamRecord};
use super::io::refill::load_wave;
use super::loops::{LeadIn, Mapping, RingLoop, apply_mapping, buffer_size_for_file};
use super::metrics::Metrics;
use super::open::{OpenError, Opened};
use super::prefetch::RegionBuffer;
use super::record::{Cmd, Effect, LoopAsk, Record, Streams, step};
use super::rt_state::RtState;
use crate::nonempty;
use crate::voice::LoopSetting;
use crate::voice::interp::Place;
use crate::voice_width;
use tutti_types::ChannelLayout;

/// Arc'd handles shared between the control side and the streamer loop.
/// `Clone` is cheap — every field is an `Arc`.
#[derive(Clone)]
pub(super) struct Handles {
    pub cache: Arc<LruCache>,
    pub metrics: Arc<Metrics>,
    /// The session rate each stream's [`SrcRatio`] is derived against.
    pub session_rate: SessionRate,
}

/// The session (graph) rate, one cell shared by the control side, the streamer
/// and every [`Status`](crate::Status) snapshot, and the streams whose ratio
/// follows it.
///
/// Shared rather than copied into each because a device restart moves it
/// (`DiskStreamer::set_sample_rate`): a copy taken at build time is how a
/// stream kept converting to 44.1 kHz on a 48 kHz device — every streamed clip
/// ~8.8% sharp and fast, with no error anywhere.
#[derive(Clone)]
pub(crate) struct SessionRate(Arc<RateCell>);

struct RateCell {
    rate: AtomicF64,
    /// Every stream whose conversion ratio follows the rate: its file's own
    /// rate, and its control cell, weakly (a stream that ends is pruned at
    /// the next call). The lock is what orders a stream's registering against
    /// a rate change, below.
    streams: Mutex<Vec<(SampleRate, Weak<RtState>)>>,
}

impl SessionRate {
    pub(crate) fn new(rate: SampleRate) -> Self {
        Self(Arc::new(RateCell {
            rate: AtomicF64::new(rate.get()),
            streams: Mutex::new(Vec::new()),
        }))
    }

    pub(crate) fn get(&self) -> SampleRate {
        SampleRate(self.0.rate.load(Ordering::SeqCst))
    }

    fn streams(&self) -> std::sync::MutexGuard<'_, Vec<(SampleRate, Weak<RtState>)>> {
        self.0
            .streams
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets `rt_state`'s conversion ratio for a file at `file_rate` against the
    /// session rate as it is now, and keeps it following the rate.
    ///
    /// The rate is read and the ratio set under the lock [`set`](Self::set)
    /// takes to store the rate and re-derive every ratio, so a stream the
    /// streamer opens concurrently either registered before the change (and
    /// is re-derived by it) or after it (and reads the new rate). Neither
    /// order leaves a stream on the old ratio. The ratio is the same derivation
    /// the in-memory tier uses (`MemorySource::set_session_sample_rate`),
    /// through the one shared constructor.
    pub(super) fn register(&self, file_rate: SampleRate, rt_state: &Arc<RtState>) {
        let mut streams = self.streams();
        streams.retain(|(_, cell)| cell.strong_count() > 0);
        rt_state.set_src_ratio(SrcRatio::for_rates(file_rate, self.get()));
        streams.push((file_rate, Arc::downgrade(rt_state)));
    }

    /// Move the rate, then re-derive the ratio of every stream that is open.
    pub(super) fn set(&self, rate: SampleRate) {
        let mut streams = self.streams();
        self.0.rate.store(rate.get(), Ordering::SeqCst);
        streams.retain(|(file_rate, cell)| match cell.upgrade() {
            Some(rt_state) => {
                rt_state.set_src_ratio(SrcRatio::for_rates(*file_rate, rate));
                true
            }
            None => false,
        });
    }
}

/// Streamer-thread-local state. Never shared. Plain data.
pub(super) struct Local {
    /// Every open stream, one record each.
    pub streams: Streams,
    /// Headroom multiplier on the refill threshold — a larger margin refills
    /// earlier and more often.
    pub buffer_margin: f64,
    /// Flat interleaved refill scratch. Streamer-thread-local, so it may grow.
    pub interleave_buffer: Vec<f32>,
}

impl Local {
    /// Thread-local state with no streams, a scratch buffer pre-sized for one
    /// `base_chunk_size` refill.
    pub(super) fn new(base_chunk_size: usize) -> Self {
        Self {
            streams: Streams::new(),
            buffer_margin: 1.0,
            interleave_buffer: Vec::with_capacity(base_chunk_size),
        }
    }
}

/// Dispatch one command on the streamer thread.
///
/// An open is answered on its own reply. Every other command is a pure
/// [`step`] over the table, and silently no-ops for a stream the streamer no
/// longer holds: commands race the streams they name, and there is no caller
/// left to report to by the time the streamer sees one.
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
        StreamerCommand::Close { stream } => {
            let _ = step(&mut local.streams, Cmd::Close(stream));
        }

        StreamerCommand::SetStreamLoop {
            stream,
            range,
            crossfade_frames,
        } => {
            let ask = LoopAsk {
                range,
                crossfade_frames,
            };
            handle_loop(stream, Some(ask), config, local);
        }
        StreamerCommand::ClearStreamLoop { stream } => handle_loop(stream, None, config, local),

        StreamerCommand::Shutdown => {}
    }
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
/// The stream is a record in the table from the moment it is built
/// ([`Phase::Opening`](super::record::Phase)) and goes live when its reader has
/// been delivered. A caller that dropped its `Opening` before the answer
/// abandoned the open, so the stream is closed again rather than fed for
/// nobody.
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
    match open_stream_region(id, file_path, start, width, shared, config) {
        Err(error) => {
            let _ = reply.send(Err(error));
        }
        Ok((record, opened)) => {
            let _ = step(&mut local.streams, Cmd::Open(Box::new(record)));
            let cmd = if reply.send(Ok(opened)).is_ok() {
                Cmd::Opened(id)
            } else {
                Cmd::Close(id)
            };
            let _ = step(&mut local.streams, cmd);
        }
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

/// Build the stream `id` on `file_path` from frame `start`: its record and
/// what its reader is made of.
///
/// Probes metadata for ring sizing and the conversion ratio *without* decoding
/// the whole file, builds the region ring at the file's own width, and pins
/// the wave in the cache for the stream's lifetime. Falls back to the whole-file `load_wave` path when the format is
/// not seekable. The placement into `width` (the file's own when `None`) is
/// built, and so refused, before anything is built.
fn open_stream_region(
    id: StreamId,
    file_path: PathBuf,
    start: u64,
    width: Option<ChannelLayout>,
    shared: &Handles,
    config: &BufferConfig,
) -> Result<(Record, Opened), OpenError> {
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

    // Pin the streamed wave in the LRU cache for the stream's lifetime. On the
    // fallback path `load_wave` inserted the whole file into the cache, so this
    // protects that resident Wave from being evicted mid-read (a fully-buffered
    // stream goes cold and would otherwise be picked as the LRU victim). On the
    // incremental-decode path the wave isn't cached, so `pin` finds no entry and
    // the guard is inert — harmless. Held by the record, so it releases when
    // the stream closes.
    let pin = shared.cache.pin(&file_path);

    let rt_state = Arc::new(RtState::new());
    shared.session_rate.register(file_sr, &rt_state);
    let fork = Arc::new(StreamRecord::new(
        file_path,
        file_sr,
        Arc::downgrade(&shared.cache),
    ));
    let origin = StreamOrigin::new(Arc::clone(&fork));
    let record = Record::new(producer, Arc::clone(&rt_state), file_length, fork, pin);
    Ok((
        record,
        Opened {
            id,
            ring,
            pos,
            rt_state,
            file_rate: file_sr,
            origin,
            place,
            width,
        },
    ))
}

/// Set (`Some(ask)`) or clear (`None`) a stream's loop: the driver's half of
/// [`Effect::Loop`], which the pure step issues for a live stream only.
///
/// The new mapping goes through [`apply_mapping`]: free when it changes no
/// frame the ring holds, else a switch the reader crosses (see `loops`'
/// module docs) — heard where the memory tier hears it when that is far
/// enough ahead, else just past the block the reader is in, crossfaded. A
/// reversed stream ignores its loop, so there the change is only stored.
///
/// Only the frames the loop needs are read — its fade's lead-in, and its body
/// when it is short (`RingLoop::capture`), through the region's decoder: a
/// loop change never decodes the whole file. A lead-in that cannot be read
/// plays the loop hard, is logged, and is what the stream's record says (so a
/// fork plays it hard too).
fn handle_loop(stream: StreamId, ask: Option<LoopAsk>, config: &BufferConfig, local: &mut Local) {
    let Some(&Effect::Loop(id, ask)) = step(&mut local.streams, Cmd::Loop(stream, ask)).first()
    else {
        return;
    };
    let Some(record) = local.streams.get_mut(id) else {
        return;
    };
    let len = record.file_frames as usize;
    let rate = record.rt_state.read_rate().get();
    let writer = &mut record.out;

    let channels = writer.channels();
    let mut recorded = ask;
    let ring_loop = ask.and_then(|ask| {
        let (ring_loop, lead_in) = RingLoop::capture(
            ask.range,
            ask.crossfade_frames,
            len,
            channels,
            &mut |at, out| writer.read_file(at, out),
        )?;
        if lead_in == LeadIn::Unreadable {
            tracing::warn!(
                "could not read the crossfade lead-in of loop {:?} in {}; \
                 the loop plays hard",
                ask.range,
                writer.file_path().display()
            );
            recorded = Some(LoopAsk {
                crossfade_frames: 0,
                ..ask
            });
        }
        Some(ring_loop)
    });
    let new = Mapping {
        ring_loop,
        ..writer.content().current.clone()
    };
    apply_mapping(writer, new, config.seek_crossfade_frames, rate);

    record.set_loop(recorded.map_or(LoopSetting::Off, |ask| LoopSetting::On {
        start: tutti_types::SamplePosition(ask.range.0 as f64),
        end: tutti_types::SamplePosition(ask.range.1 as f64),
        crossfade_frames: ask.crossfade_frames,
    }));
}
