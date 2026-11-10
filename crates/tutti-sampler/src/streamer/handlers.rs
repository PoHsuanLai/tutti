//! Streamer command handlers + their state types.
//!
//! `Handles` — Arc'd state also held by the driver.
//! `Local`    — streamer-thread-local data (the stream table, scratch).
//! `handle_command` — the driver of [`step`]: it does the file work a command
//! needs, runs the pure step over the table, and performs the effects.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use atomic_float::AtomicF64;
use std::sync::atomic::Ordering;
use tutti_types::{SampleRate, SrcRatio};

use super::StreamerSender;
use super::cache::LruCache;
use super::command::{Decoded, Reply, StreamId, StreamerCommand};
use super::config::BufferConfig;
use super::control::{StreamOrigin, StreamRecord};
use super::decode::{DecodeLane, Seams, panic_message};
use super::loops::{LeadIn, Mapping, RingLoop, apply_mapping, buffer_size_for_file};
use super::metrics::Metrics;
use super::open::{OpenError, Opened};
use super::prefetch::RegionBuffer;
use super::record::{Cmd, Effect, LoopAsk, Record, Streams, step};
use super::rt_state::RtState;
use super::status::StatusCell;
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
    /// What a test replaces (production: none).
    pub seams: Seams,
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
    /// The thread whole-file decodes run on, started on first use.
    pub lane: DecodeLane,
    /// Opens waiting on the lane, until their `Decoded` arrives.
    pub pending: Vec<Pending>,
}

/// An open of a file that cannot seek, waiting for the decode lane.
pub(super) struct Pending {
    id: StreamId,
    file_path: PathBuf,
    start: u64,
    width: Option<ChannelLayout>,
    reply: Reply,
}

impl Local {
    /// Thread-local state with no streams, a scratch buffer pre-sized for one
    /// `base_chunk_size` refill. `reports` is the queue the decode lane
    /// answers on.
    pub(super) fn new(base_chunk_size: usize, reports: StreamerSender) -> Self {
        Self {
            streams: Streams::new(),
            buffer_margin: 1.0,
            interleave_buffer: Vec::with_capacity(base_chunk_size),
            lane: DecodeLane::new(reports),
            pending: Vec::new(),
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
        } => {
            let open = Pending {
                id,
                file_path,
                start,
                width,
                reply,
            };
            handle_open(open, shared, config, local);
        }
        StreamerCommand::Decoded {
            id,
            wave: Decoded(wave),
        } => {
            if let Some(at) = local.pending.iter().position(|open| open.id == id) {
                let open = local.pending.swap_remove(at);
                let source = wave.map(Source::Whole);
                complete(open, source, shared, config, local);
            }
        }
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
/// Returns `None` — signalling the whole-file fallback, on the decode lane —
/// when a codec feature isn't compiled in, the probe fails, or the format is
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

/// The incremental decoder a seekable stream reads through; none without a
/// codec.
#[cfg(codec)]
type Decoder = tutti_codec::FileIn;
#[cfg(not(codec))]
type Decoder = std::convert::Infallible;

/// What a stream's ring is fed from: a decoder that seeks in the file, or the
/// file decoded whole.
enum Source {
    #[cfg(codec)]
    Stream(tutti_codec::WaveMetadata, Box<tutti_codec::FileIn>),
    Whole(Arc<tutti_codec::Wave>),
}

/// Where an open stands after its probe.
enum Begun {
    /// Its source is in hand.
    Ready(Source),
    /// The file is being decoded whole on the decode lane; the open waits for
    /// its `Decoded`.
    #[cfg_attr(
        not(codec),
        allow(dead_code, reason = "with no decoder a miss fails at once")
    )]
    Decoding,
}

/// Probes `file_path` for how to feed stream `id`: a seeking decoder, a wave
/// the cache already holds, or else a whole-file decode on the decode lane.
/// The streamer's thread reads a header here and nothing more.
fn begin(
    id: StreamId,
    file_path: &Path,
    shared: &Handles,
    lane: &mut DecodeLane,
) -> Result<Begun, OpenError> {
    // Probe metadata (frame count / sample rate) for ring sizing + src_ratio
    // WITHOUT decoding the whole file. Prefer real incremental streaming; fall
    // back to the whole-file path when the format isn't seekable (no frame
    // count) or opening the stream decoder fails.
    #[cfg(codec)]
    if let Some((meta, decoder)) = open_stream(file_path) {
        return Ok(Begun::Ready(Source::Stream(meta, Box::new(decoder))));
    }
    if let Some(wave) = shared.cache.get(file_path) {
        return Ok(Begun::Ready(Source::Whole(wave)));
    }
    // With no decoder compiled in, a miss has nowhere to be filled.
    #[cfg(not(codec))]
    {
        let _ = (id, lane);
        Err(super::decode::unreadable(file_path))
    }
    #[cfg(codec)]
    {
        // A file that is not there is no job: it fails now, where the lane
        // would only fail it later.
        std::fs::File::open(file_path).map_err(|error| OpenError::Io(error.kind()))?;
        lane.submit(shared, id, file_path.to_path_buf())?;
        Ok(Begun::Decoding)
    }
}

/// Open stream `open.id`, its window starting at frame `open.start`, and answer
/// on `open.reply`: the reader's parts, or why there is none.
///
/// A file that cannot seek waits for the decode lane (`Pending`); everything
/// else is built here, now. The stream is a record in the table from the
/// moment it is built ([`Phase::Opening`](super::record::Phase)) and goes live
/// before its reply is sent, so a handle never reads `Opening`. A caller that
/// dropped its `Opening` before the answer abandoned the open, so the stream
/// is closed again rather than fed for nobody.
///
/// The probe and the build run under `catch_unwind`: a decoder that panics on
/// a file fails that open (`OpenError::Format`), not the streamer.
fn handle_open(open: Pending, shared: &Handles, config: &BufferConfig, local: &mut Local) {
    let begun = catch_unwind(AssertUnwindSafe(|| {
        begin(open.id, &open.file_path, shared, &mut local.lane)
    }));
    match begun {
        Ok(Ok(Begun::Ready(source))) => complete(open, Ok(source), shared, config, local),
        Ok(Ok(Begun::Decoding)) => local.pending.push(open),
        Ok(Err(error)) => complete(open, Err(error), shared, config, local),
        Err(payload) => {
            tracing::error!(
                "opening {} panicked: {}",
                open.file_path.display(),
                panic_message(payload.as_ref())
            );
            complete(open, Err(OpenError::Format), shared, config, local);
        }
    }
}

/// Answers `open` from `source`: builds its stream, puts the record in the
/// table and replies with the reader's parts, or replies with the failure.
fn complete(
    open: Pending,
    source: Result<Source, OpenError>,
    shared: &Handles,
    config: &BufferConfig,
    local: &mut Local,
) {
    let Pending {
        id,
        file_path,
        start,
        width,
        reply,
    } = open;
    let built = source.and_then(|source| {
        catch_unwind(AssertUnwindSafe(|| {
            build_stream(id, file_path.clone(), start, width, source, shared, config)
        }))
        .unwrap_or_else(|payload| {
            tracing::error!(
                "building the stream of {} panicked: {}",
                file_path.display(),
                panic_message(payload.as_ref())
            );
            Err(OpenError::Format)
        })
    });
    match built {
        Err(error) => {
            let _ = reply.send(Err(error));
        }
        Ok((record, opened)) => {
            // The stream is live before its handle exists, so a handle never
            // reads `Opening`; a caller that has gone is a close.
            let _ = step(&mut local.streams, Cmd::Open(Box::new(record)));
            let _ = step(&mut local.streams, Cmd::Opened(id));
            if reply.send(Ok(opened)).is_err() {
                let _ = step(&mut local.streams, Cmd::Close(id));
            }
        }
    }
}

/// Build the stream `id` on `file_path` from frame `start`: its record and
/// what its reader is made of.
///
/// Builds the region ring at the file's own width and pins the wave in the
/// cache for the stream's lifetime. The placement into `width` (the file's own
/// when `None`) is built, and so refused, before anything is built.
fn build_stream(
    id: StreamId,
    file_path: PathBuf,
    start: u64,
    width: Option<ChannelLayout>,
    source: Source,
    shared: &Handles,
    config: &BufferConfig,
) -> Result<(Record, Opened), OpenError> {
    let (file_length, file_sr, file_topology, decoder, resident): (_, _, _, Option<Decoder>, _) =
        match source {
            // Decoder metadata carries the header's integer rate.
            #[cfg(codec)]
            Source::Stream(meta, decoder) => (
                meta.total_frames.unwrap_or(0),
                SampleRate::from(meta.sample_rate),
                decoder.topology().clone(),
                Some(*decoder),
                None,
            ),
            Source::Whole(wave) => (
                wave.len() as u64,
                wave.sample_rate(),
                wave.topology().clone(),
                None,
                Some(wave),
            ),
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
    let (mut producer, ring, pos) = RegionBuffer::for_file(
        id,
        file_path.clone(),
        buffer_capacity,
        file_topology,
        file_length as usize,
    );

    // `mut` is used by the `set_decoder` call, which every codec feature
    // gates — so a build with none of them on sees an unused `mut` rather than
    // a dead binding.
    #[cfg(codec)]
    if let Some(decoder) = decoder {
        producer.set_decoder(decoder);
    }
    #[cfg(not(codec))]
    let _ = decoder;
    #[cfg(test)]
    if let Some(hook) = &shared.seams.read_hook {
        producer.set_read_hook(Arc::clone(hook));
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
    // whole-file path the decode lane inserted the file into the cache, so this
    // protects that resident Wave from being evicted mid-read (a fully-buffered
    // stream goes cold and would otherwise be picked as the LRU victim). On the
    // incremental-decode path the wave isn't cached, so `pin` finds no entry and
    // the guard is inert — harmless. Held by the record, so it releases when
    // the stream closes.
    let pin = shared.cache.pin(&file_path);

    let rt_state = Arc::new(RtState::new());
    shared.session_rate.register(file_sr, &rt_state);
    let status = Arc::new(StatusCell::new());
    let fork = Arc::new(StreamRecord::new(
        file_path,
        file_sr,
        Arc::downgrade(&shared.cache),
    ));
    let origin = StreamOrigin::new(Arc::clone(&fork));
    let record = Record::new(
        producer,
        Arc::clone(&rt_state),
        file_length,
        fork,
        pin,
        Arc::clone(&status),
    );
    Ok((
        record,
        Opened {
            id,
            ring,
            pos,
            rt_state,
            status,
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
    // The capture reads the file, so it runs contained like a refill.
    let _ = record.contained(|record| capture_loop(record, ask, config));
    local.streams.reap();
}

/// Captures `ask` into `record`'s ring and records it as applied: the file
/// work of [`handle_loop`].
fn capture_loop(record: &mut Record, ask: Option<LoopAsk>, config: &BufferConfig) {
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
