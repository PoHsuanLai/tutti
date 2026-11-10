//! Disk streaming sample playback, fed by the streamer thread.
//!
//! Two types, one tier: [`DiskSource`] is the reader of the streamer's ring (the
//! ring, its control cell and its width), and [`DiskVoice`] is a timeline clip
//! over it: it places its read on the transport exactly as the memory tier's
//! placed read does (`interp::place`, the same gate and step, from the block's
//! `Env`) and reads the ring at that position through `LiveRead`: the ring
//! indexed by position, the memory tier's tap layout, the
//! one kernel — so a clip plays what the same clip in memory plays at the same
//! clock frame, and a jump is a crossfade from where it was, not a flush.
//!
//! Every count crossing the ring is in frames. Nothing on the `process`
//! path allocates, locks, or blocks — but a fork's: it reads its file
//! (`offline_read`), which is why a disk voice forks only for an offline
//! render.

use std::ops::Range;
use std::sync::Arc;

use crate::MAX_SAMPLER_CHANNELS;
use tutti_graph::{
    Cx, ForkCause, ForkFaultKind, ForkHealth, ForkMode, ForkSource, Forked, IntoNode, Io, Node,
    NodeParts, Prepare, Shape, Status,
};
use tutti_types::{
    Amplitude, Beat, BeatDuration, ChannelLayout, PlaybackRate, ReadRate, SamplePosition,
    SampleRate, SrcRatio, Tail,
};

use super::clock::{BlockClock, Clock};
use super::fault::FaultLatch;
use super::interp::{Gate, Place, past_window, place};
use super::live_read::{LiveRead, Out};
use super::memory_source::VoiceWindow;
use super::offline_read::OfflineRead;
use super::types::Direction;
use crate::lanes::{Gather, Lanes, UNPREPARED_FRAMES, audible};
use crate::streamer::{Cells, SharedReader, StatusCell, StreamFile, StreamId, StreamReader};
use tutti_transport::TempoReader;

/// The streaming tier's bare reader: the streamer's ring, its control cell
/// and its width. A [`DiskVoice`] wraps one and reads the ring by the clock.
///
/// The audio thread holds the ring through a `SharedReader` (an `Arc`) and
/// the ring's one `PosReader`, which this source owns and a clone does not
/// get (`LiveRead`'s module docs): every access is an atomic.
///
/// It is not a graph node: it knows neither where on the timeline it plays
/// nor its file. A [`DiskVoice`] is, and forks.
pub struct DiskSource {
    /// Boxed: a voice in a pool should not carry the reader's jump state
    /// inline (it is built on the control thread, where the box is).
    read: Box<LiveRead>,

    /// The stream's scalar cells (speed, stretch, gain, conversion ratio,
    /// underruns): `None` in a severed copy.
    shared_state: Option<Arc<Cells>>,

    /// Output width — the declaration.
    channels: ChannelLayout,
    /// `channels.count()`, cached.
    stride: usize,
}

// Hand-rolled: `read` holds the ring and `shared_state` an `Arc<Cells>`.
impl std::fmt::Debug for DiskSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskSource")
            .field("has_shared_state", &self.shared_state.is_some())
            .finish_non_exhaustive()
    }
}

impl Clone for DiskSource {
    fn clone(&self) -> Self {
        Self {
            read: self.read.clone(),
            shared_state: self.shared_state.clone(),
            channels: self.channels,
            stride: self.stride,
        }
    }
}

impl DiskSource {
    /// A source over `consumer` through its one position `reader` (test rings),
    /// at the ring's own width.
    #[cfg(test)]
    pub(crate) fn new(
        consumer: SharedReader,
        reader: tutti_rt::PosReader,
        shared_state: Arc<Cells>,
    ) -> Self {
        let width = crate::voice_width(consumer.channels());
        let place = Place::for_topology(consumer.topology(), width)
            .expect("a test ring is placeable at its own width");
        let control = consumer.control_reader();
        Self::with_reader(
            consumer,
            Some((reader, control)),
            shared_state,
            width,
            place,
        )
    }

    /// A source over `consumer` through its one `reader` and the reader of its
    /// map (`None`: silent), `width` wide (at most [`MAX_SAMPLER_CHANNELS`],
    /// the widest frame the read path stacks), placing the ring's channels by
    /// `place`: the [`Place`] of the ring's topology into `width`
    /// ([`Place::for_topology`], built, and so refusing, when the stream was
    /// opened). The file's own width and order give the identity.
    pub(crate) fn with_reader(
        consumer: SharedReader,
        feed: Option<(
            tutti_rt::PosReader,
            tutti_rt::Reader<crate::streamer::RingControl>,
        )>,
        shared_state: Arc<Cells>,
        width: ChannelLayout,
        place: Place,
    ) -> Self {
        let channels = crate::voice_width(width);
        let stride = channels.count() as usize;
        Self {
            read: Box::new(LiveRead::new(consumer, feed, stride, place)),
            shared_state: Some(shared_state),
            channels,
            stride,
        }
    }

    /// Publishes a new output gain (NaN or infinite: silence), to the stream's `Cells` (see `tutti_nodes`'
    /// crate docs for why a control is never a field).
    pub fn set_gain(&self, gain: Amplitude) {
        if let Some(ref state) = self.shared_state {
            state.set_gain(gain);
        }
    }

    /// The current output gain; unity with no shared state.
    pub fn gain(&self) -> Amplitude {
        self.shared_state
            .as_ref()
            .map_or(Amplitude::new(1.0), |s| s.gain())
    }

    /// Output width — this unit's `outputs()`.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// Forgets the position and any fade: the next block starts afresh.
    pub fn reset_interpolation(&mut self) {
        self.read.reset();
    }
}

impl DiskSource {
    /// Let go of the live stream: the ring's reader, and
    /// the control cell. A copy severed so never touches the live stream.
    fn sever(&mut self) {
        self.shared_state = None;
        self.reset_interpolation();
        self.read.sever();
    }

    /// Output width, as a count.
    #[inline]
    fn width(&self) -> usize {
        self.stride
    }
}

// ---------------------------------------------------------------------------
// DiskVoice — a timeline clip over a stream.
//
// Per frame: the placement (`interp::place`, the memory tier's) gives the
// position the transport puts the playhead at inside the window, `None`
// outside it or on a standing transport; the ring is read there. The streamer
// fills ahead of where the voice reads; a jump is the reader's own crossfade
// (`LiveRead`).
// ---------------------------------------------------------------------------

/// Wiring for [`DiskVoice::new`]: the window plus the file sample rate.
/// `shared_state` stays a separate wiring arg (it must be the same `Cells`
/// the `inner` unit holds). The transport is not wiring: a voice reads it
/// from each block's `Env`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DiskVoiceConfig {
    /// Span of timeline this voice occupies. Mirrors `MemorySource` — see
    /// [`VoiceWindow`].
    pub window: VoiceWindow,
    /// File sample rate — converts the transport's second-offset into a file
    /// position, matching `MemorySource`'s use of `wave.sample_rate()`.
    ///
    /// The rate the streamer recorded from the file's header when it opened the
    /// stream (`StreamReader::file_rate`), not one recovered from the session
    /// rate: that one moves on a device restart, and the file's does not.
    pub file_sample_rate: SampleRate,
}

/// A [`DiskSource`]'s stream, placed on the timeline.
///
/// A timeline clip must sound only while the playhead is inside its
/// `[start, start + duration)` window, and there play the file frame the
/// transport puts the playhead on. This does exactly what the memory tier's
/// placed read does — the same gate ([`window_position`](super::interp::window_position))
/// placed and stepped the same way (`interp::place`), from the block's `Env`
/// — and reads the ring at the position that gives, so live, forked and
/// in-memory voices of one clip play the same samples at the same frame.
///
/// # A fork reads the file itself
///
/// A fork for an offline render (an export) cannot read the ring: its reads
/// would move the live voice's window. So the fork (`fork_copy`,
/// a graph fork's source for this node and for a `VoiceNode` or pool holding
/// it) is cut off from the stream, and handed the stream's file, as the
/// streamer records it (path, rate, loop), to read on demand on the render's
/// thread. It plays the same window of the file on the render's transport
/// (it reads its render's `Env`), read as the memory tier reads it; see
/// `offline_read`. That path blocks on file I/O and is never taken live: a
/// live fork of a disk voice is refused.
pub struct DiskVoice {
    inner: DiskSource,
    /// The playback controls: the streamer's cell, shared with `inner`, while
    /// live; a private snapshot of it in a fork.
    shared_state: Arc<Cells>,

    /// Span of timeline this voice occupies.
    window: VoiceWindow,

    /// File sample rate — converts the transport's second-offset into a file
    /// position, matching `MemorySource`'s use of `wave.sample_rate()`.
    file_sample_rate: SampleRate,

    /// The stream this voice consumes, as what a fork reads: the file it
    /// plays and whether the stream is still served. `None` for a voice not
    /// built by [`DiskVoice::from_reader`] (a test's bare ring), whose fork
    /// then plays silence.
    origin: Option<Origin>,

    /// `Some` in a fork ([`fork_copy`](Self::fork_copy)): it never touches
    /// the ring or the streamer, and plays what this holds instead. Boxed: a
    /// live voice (every voice in a pool) should not carry the pages' room,
    /// and it is built on the control thread, where a fork is taken.
    offline: Option<Box<Offline>>,

    /// The rate `prepare` last gave this voice, `None` until one did: the
    /// rate the step converts to. A fork never told a rate renders nothing
    /// and says so rather than play off pitch; a live voice never told one
    /// steps by the streamer's conversion for the session rate.
    sample_rate: Option<SampleRate>,

    /// The transport as this voice reads it when it is a graph node of its
    /// own (a voice in a pool or a `VoiceNode` reads its owner's).
    clock: Clock,

    /// The project's tempo map, when this voice reads one: lent once per
    /// block to its clock ([`with_tempo`](Self::with_tempo)).
    tempo: Option<TempoReader>,

    /// A block's positions when this voice is a graph node of its own,
    /// sized from `Prepare::max_block` in `prepare` (empty until then and in
    /// a clone, so a voice that only ever plays in a pool or a `VoiceNode`,
    /// which read through their own scratch, carries none).
    positions: Vec<Option<SamplePosition>>,
}

/// What a live voice keeps of its stream for a fork to read.
///
/// Read-only: nothing reachable through it can drive the stream. The file is
/// immutable and the status says whether the streamer's record is gone; the
/// loop as applied and the direction come from the ring's published control,
/// when the fork is taken (a loop set later on the stream is not a copy this
/// would have missed).
#[derive(Clone)]
struct Origin {
    file: Arc<StreamFile>,
    status: Arc<StatusCell>,
}

/// A forked disk voice's own playback: the file it reads, the direction it
/// reads in, and where its failures go.
#[derive(Clone, Debug, Default)]
struct Offline {
    /// The direction the stream played in when the fork was taken.
    direction: Direction,
    /// The file, from the streamer's record when the fork was taken. `None`
    /// when the stream was gone by then: silence.
    read: Option<OfflineRead>,
    /// The first failure since this copy was forked (its stream gone, its
    /// file unreadable, no render rate), what the fork's health probe
    /// reports ([`fault`](DiskVoice::fault)). Fresh at every fork, so a copy
    /// never reports another's.
    fault: Arc<FaultLatch>,
}

/// Why a forked disk voice renders silence where its file should be, other
/// than the file itself (`OfflineReadError`).
#[derive(Debug)]
enum OfflineFault {
    /// Its stream ended (stopped, or its channel restarted on another file)
    /// before the copy was forked.
    StreamGone,
    /// It was asked to render before it was prepared with a sample rate.
    NoRenderRate,
}

impl std::fmt::Display for OfflineFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::StreamGone => {
                "its disk stream had ended (stopped, or its channel restarted on another \
                 file) when it was forked, so there was no file to play"
            }
            Self::NoRenderRate => "it was rendered before it was given a sample rate",
        })
    }
}

impl std::error::Error for OfflineFault {}

/// Why a disk voice refused a fork.
#[derive(Debug)]
pub(crate) struct LiveDiskFork;

impl std::fmt::Display for LiveDiskFork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(
            "a disk voice forks only for an offline render: a live copy cannot read the \
             stream's ring, and reading its file would block the audio thread",
        )
    }
}

impl std::error::Error for LiveDiskFork {}

/// A forked voice's failure latch, as the fork's health probe: a failure it
/// latched is [`ForkFaultKind::Failed`].
pub(crate) struct VoiceHealth(pub(crate) Arc<FaultLatch>);

impl ForkHealth for VoiceHealth {
    fn fault(&self) -> Option<(ForkFaultKind, ForkCause)> {
        self.0
            .fault()
            .map(|e| (ForkFaultKind::Failed, ForkCause::from_arc(e)))
    }
}

impl std::fmt::Debug for DiskVoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskVoice")
            .field("inner", &self.inner)
            .field("window", &self.window)
            .field("file_sample_rate", &self.file_sample_rate)
            .field("has_origin", &self.origin.is_some())
            .field("offline", &self.offline)
            .finish_non_exhaustive()
    }
}

impl Clone for DiskVoice {
    /// A copy sharing the stream (the ring, without its reader: see
    /// `LiveRead`) and the control cell. Not a fork: see
    /// `fork_copy`.
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared_state: Arc::clone(&self.shared_state),
            window: self.window,
            file_sample_rate: self.file_sample_rate,
            origin: self.origin.clone(),
            offline: self.offline.clone(),
            sample_rate: self.sample_rate,
            clock: Clock::new(),
            tempo: self.tempo.clone(),
            // Unsized, so a clone stays allocation-free as it was: a copy
            // inserted as a node is prepared, which sizes it.
            positions: Vec::new(),
        }
    }
}

impl DiskVoice {
    /// Places a `DiskSource`'s stream on the timeline.
    ///
    /// Construction (streamer stream registration, ring allocation) happens on
    /// the ECS/streamer side; this only binds the already-built unit to a
    /// timeline window. `shared_state` must be the same `Cells` the `inner`
    /// unit holds.
    pub fn new(inner: DiskSource, shared_state: Arc<Cells>, config: DiskVoiceConfig) -> Self {
        Self {
            inner,
            shared_state,
            window: config.window,
            file_sample_rate: config.file_sample_rate,
            origin: None,
            offline: None,
            sample_rate: None,
            clock: Clock::new(),
            tempo: None,
            positions: Vec::new(),
        }
    }

    /// Builds the voice that plays a stream, placed on the timeline at
    /// `start_beat` for `duration` (the transport it reads is its block's,
    /// from the graph). The reader is the stream's one, moved in: it is the
    /// reply of the open ([`DiskStreamer::open`](crate::DiskStreamer::open)),
    /// so it cannot be built into a second voice.
    ///
    /// `start_beat` and `duration` are musical time; the gate converts them to
    /// file frames using the file's own rate ([`StreamReader::file_rate`], the
    /// rate the streamer recorded when it opened the stream), so a session
    /// rate that moves later (a device restart) leaves the gate's frames where
    /// they were. The voice is as wide as the open asked for, and plays the
    /// file's channels where they are unless the file's order is not the
    /// engine's (a WAV 7.1): that was permuted to it when the stream opened.
    ///
    /// The voice also keeps the stream's file, and whether the stream is still
    /// served, so a fork of it for an offline render (an export) can read the
    /// same file on its own rather than through the live ring; see
    /// `DiskVoice::fork_copy`.
    ///
    /// Control thread; the returned voice is then moved into the graph.
    pub fn from_reader(
        reader: StreamReader,
        start_beat: tutti_types::Beat,
        duration: Option<tutti_types::BeatDuration>,
    ) -> Self {
        let StreamReader {
            ring,
            pos,
            control,
            cells,
            file,
            status,
            place,
            width,
            ..
        } = reader;
        let file_rate = file.file_rate();
        let inner =
            DiskSource::with_reader(ring, Some((pos, control)), Arc::clone(&cells), width, place);
        let mut voice = Self::new(
            inner,
            cells,
            DiskVoiceConfig {
                window: VoiceWindow {
                    start: start_beat,
                    duration,
                },
                file_sample_rate: file_rate,
            },
        );
        voice.origin = Some(Origin { file, status });
        voice
    }

    /// The streamer stream this voice reads: its ring's stream, which a loop
    /// forwarded for the voice names so the streamer ignores it once the
    /// stream is gone.
    pub(crate) fn stream(&self) -> StreamId {
        self.inner.read.ring().region_id()
    }

    /// The voice's window on the timeline.
    pub fn window(&self) -> VoiceWindow {
        self.window
    }

    /// A copy of this voice for an offline render, sharing nothing with
    /// it: cut off from the stream (no ring reader, no control cell — a
    /// private snapshot of the controls' values, `Cells::detached`), and
    /// handed the stream's file to read itself, with the loop and the
    /// direction the streamer has applied now (read from the ring's published
    /// control: a loop it downgraded is the downgrade, not the request). A
    /// stream that has ended by now is a latched failure
    /// ([`fault`](Self::fault)): the export fails naming the voice rather than
    /// write its silence.
    ///
    /// Reads the ring's control cell (a lock, and an owning `Arc` that frees
    /// here): control thread only, as every fork is.
    pub(crate) fn fork_copy(&self) -> Self {
        let mut copy = self.clone();
        let applied = self.inner.read.ring().applied();
        copy.inner.sever();
        copy.shared_state = Arc::new(self.shared_state.detached());
        copy.clock = Clock::new();
        let fault = Arc::new(FaultLatch::default());
        let (read, direction) = match &self.origin {
            Some(origin) if origin.status.closed() => {
                fault.latch(OfflineFault::StreamGone);
                (None, applied.direction)
            }
            Some(origin) => (
                Some(OfflineRead::new(
                    Arc::clone(&origin.file),
                    applied.loop_,
                    Arc::clone(&fault),
                )),
                applied.direction,
            ),
            // A fork of a fork keeps the file it was handed; a voice over a
            // bare ring has none.
            None => match self.offline.as_ref() {
                Some(o) => (
                    o.read
                        .clone()
                        .map(|read| read.relatched(Arc::clone(&fault))),
                    o.direction,
                ),
                None => (None, applied.direction),
            },
        };
        copy.origin = None;
        copy.offline = Some(Box::new(Offline {
            read,
            direction,
            fault,
        }));
        copy
    }

    /// The loop a fork plays: the loop the stream had applied when it was
    /// forked (tests). `None` live, or for a fork with no file.
    #[cfg(test)]
    pub(crate) fn forked_loop(&self) -> Option<super::memory_source::LoopSetting> {
        self.offline
            .as_ref()?
            .read
            .as_ref()
            .map(OfflineRead::loop_setting)
    }

    /// A fork's failure latch; `None` live.
    pub(crate) fn fault(&self) -> Option<Arc<FaultLatch>> {
        self.offline.as_ref().map(|o| Arc::clone(&o.fault))
    }

    /// Tells the stream how fast a wrapping time-stretcher wants its source.
    ///
    /// `1 / stretch`, or [`ReadRate::UNITY`] when nothing wraps this voice. See
    /// `Cells::read_rate` for why the factor lands there rather than at the
    /// per-sample advance.
    ///
    /// Callers must publish unity when they stop stretching. The rate
    /// belongs to the filter, not to the voice, so a voice returned to 1.0x
    /// that never re-published would keep reading at the old factor. Both
    /// branches of the pool's read set it every block for exactly that reason.
    #[inline]
    pub fn set_stretch_rate(&self, rate: ReadRate) {
        self.shared_state.set_stretch_rate(rate);
    }

    /// Moves the voice's window to `[start_beat, start_beat + duration)`, or to
    /// the whole source when `duration` is `None`. The next frame reads at the
    /// new window.
    pub fn set_placement(&mut self, start_beat: Beat, duration: Option<BeatDuration>) {
        self.window = VoiceWindow {
            start: start_beat,
            duration,
        };
    }

    /// Publishes a new output gain (a NaN or infinite one is silence). `&self`: the write lands in the shared
    /// `Cells`, so no exclusivity is needed.
    pub fn set_gain(&self, gain: Amplitude) {
        self.shared_state.set_gain(gain);
    }

    /// The current output gain, read from the shared state — so a clone reports
    /// what the frontend last published, not what it was cloned at.
    pub fn gain(&self) -> Amplitude {
        self.shared_state.gain()
    }

    /// The file's own sample rate (the gate's rate: it measures the clock in
    /// the file's frames).
    pub fn file_sample_rate(&self) -> SampleRate {
        self.file_sample_rate
    }

    /// Sets the playback speed magnitude, in the stream's `Cells`. The next block reads at the
    /// position the new speed gives, crossfaded.
    pub fn set_speed(&mut self, speed: PlaybackRate) {
        self.shared_state.set_speed(speed);
    }

    /// Output width, as a count.
    #[inline]
    pub(crate) fn width(&self) -> usize {
        self.inner.width()
    }

    /// The rate the gate converts the clock at, relative to the file's frames:
    /// varispeed and the stretcher's rate, no conversion (the gate measures in
    /// the file's own frames).
    #[inline]
    fn window_rate(&self) -> ReadRate {
        self.shared_state
            .effective_speed()
            .read_rate(SrcRatio::UNITY)
            .then(self.shared_state.stretch_rate())
    }

    /// File frames per output frame: varispeed and the stretcher's rate, with
    /// the conversion from the file's rate to the rate this unit renders at —
    /// derived from the two rates, as the memory tier derives it. A live voice
    /// never told a rate takes the streamer's conversion for the session.
    #[inline]
    fn step_rate(&self) -> ReadRate {
        let (speed, stretch) = (
            self.shared_state.effective_speed(),
            self.shared_state.stretch_rate(),
        );
        match self.sample_rate {
            Some(render_rate) => speed
                .read_rate(SrcRatio::for_rates(self.file_sample_rate, render_rate))
                .then(stretch),
            None if self.offline.is_some() => ReadRate::UNITY,
            None => self.shared_state.read_rate(),
        }
    }

    /// The gate: this voice's window, in the file's frames, at the window
    /// rate.
    #[inline]
    fn gate(&self) -> Gate {
        Gate {
            window: self.window,
            source_rate: self.file_sample_rate,
            rate: self.window_rate(),
        }
    }

    /// Render block frames `range` (at most the lanes' length) into frames
    /// `0..range.len()` of the first `n` lanes: the voice's own channels,
    /// then silence on any lane past them. The live read claims the ring
    /// once per run of the block (a transport change or a loop wrap starts a
    /// run); a fork reads its file frame by frame. The block read of a slot
    /// holding this voice (`PlaybackSlot::render_lanes`), through the
    /// owner's position scratch (`positions`, at least `range.len()` long).
    ///
    /// Returns `false` when no frame of a live voice has a position (its
    /// window is closed, or the transport stands): every frame is +0.0, the
    /// lanes are not written and the ring is not read. The reader's
    /// state is left as a block of silent frames leaves it, so the next
    /// sounding block starts where it would have had the silence been
    /// rendered. A fork always returns `true`.
    pub(crate) fn render_lanes(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        lanes: &mut Lanes,
        n: usize,
        positions: &mut [Option<SamplePosition>],
        gather: &mut Gather,
    ) -> bool {
        let w = self.width().min(n);
        let frames = range.len();
        if self.offline.is_some() {
            lanes.clear(n, frames);
            self.offline_render(clock, range, positions, |i, f| lanes.put(i, &f[..w]));
            return true;
        }
        let sounds = self.live_place(clock, range.clone(), positions);
        if !sounds && !force_no_skip() {
            self.inner.read.silence();
            return false;
        }
        lanes.clear(n, frames);
        let mut out: Out<'_, fn(usize, &[f32])> = Out::Lanes {
            lanes: &mut lanes.lanes_mut()[..w],
            gather,
        };
        self.live_read(clock, range, positions, &mut out);
        true
    }

    /// Render live frames `range` without the silent-block skip (the
    /// callers that always render), frame `i - range.start` handed to `emit`:
    /// the placed positions, read from the ring,
    /// one ring read per run (its generation the read's segment, so a jump
    /// is crossfaded). `positions` is scratch, at least `range.len()` long.
    fn live_render<F: FnMut(usize, &[f32])>(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
        out: &mut Out<'_, F>,
    ) {
        self.live_place(clock, range.clone(), positions);
        self.live_read(clock, range, positions, out);
    }

    /// The straight positions of live frames `range` in `positions[..len]`;
    /// whether any frame has one. The compiler aligns the voice in the graph:
    /// the stream reads no earlier or later than its clock.
    fn live_place(
        &self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
    ) -> bool {
        let positions = &mut positions[..range.len()];
        place(clock, range, self.gate(), self.step_rate(), positions)
    }

    /// Read the ring at the positions [`live_place`](Self::live_place) left,
    /// one read per run of `range`.
    fn live_read<F: FnMut(usize, &[f32])>(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &[Option<SamplePosition>],
        out: &mut Out<'_, F>,
    ) {
        let rate = self.step_rate();
        let gain = audible(self.shared_state.gain().get());
        // Borrowed beside `inner`, not cloned: `self` holds a strong
        // reference, so a clone only adds two atomic refcount updates.
        let state = &*self.shared_state;
        let base = range.start;
        for run in clock.runs_in(range) {
            let (a, b) = (run.start, run.end);
            self.inner.read.render(
                &positions[a - base..b - base],
                run.generation,
                rate.get(),
                gain,
                state,
                a - base,
                out,
            );
        }
    }

    /// A fork's frames `range`, frame `i - range.start` handed to `emit`.
    ///
    /// The placement is the live one; where the live voice reads the ring,
    /// this reads the file at the placed position. Once the playhead is past
    /// the window's end, the file is closed: a render holding many voices
    /// keeps a file open per voice sounding. `positions` is scratch, at
    /// least `range.len()` long.
    fn offline_render(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
        mut emit: impl FnMut(usize, &[f32]),
    ) {
        let w = self.width().min(MAX_SAMPLER_CHANNELS);
        let silent = [0.0f32; MAX_SAMPLER_CHANNELS];
        let frames = range.len();
        let gate = self.gate();
        let rate = self.step_rate();
        let gain = audible(self.shared_state.gain().get());
        let told_rate = self.sample_rate.is_some();
        place(clock, range.clone(), gate, rate, &mut positions[..frames]);
        let past = past_window(clock, range, gate);
        let Some(offline) = self.offline.as_mut() else {
            (0..frames).for_each(|i| emit(i, &silent[..w]));
            return;
        };
        let direction = offline.direction;
        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
        for (i, pos) in positions[..frames].iter().enumerate() {
            let Some(pos) = pos else {
                emit(i, &silent[..w]);
                continue;
            };
            // A copy never told a rate latches, inside its window.
            if !told_rate {
                offline.fault.latch(OfflineFault::NoRenderRate);
                emit(i, &silent[..w]);
                continue;
            }
            match offline.read.as_mut() {
                Some(read) => {
                    read.read_into(*pos, direction, self.inner.read.place(), &mut frame[..w]);
                }
                None => frame[..w].fill(0.0),
            }
            for s in frame[..w].iter_mut() {
                *s *= gain;
            }
            emit(i, &frame[..w]);
        }
        if past && let Some(read) = offline.read.as_mut() {
            read.close();
        }
    }

    /// Run at `sample_rate`: the step converts the file's rate to it. What
    /// [`Node::prepare`] does; allocation-free.
    pub(crate) fn set_render_rate(&mut self, sample_rate: SampleRate) {
        self.sample_rate = Some(sample_rate);
    }

    /// Forget the read's jump state (a slot's flush at a jump, a reset).
    pub(crate) fn flush(&mut self) {
        self.inner.reset_interpolation();
    }

    /// Block frames `range` of this voice as a node of its own into `out`
    /// (its first `out.len()` channels), through `positions` (at least
    /// `range.len()` long).
    fn render_piece(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
        out: &mut [&mut [f32]],
    ) {
        let base = range.start;
        let emit = |i: usize, f: &[f32]| {
            for (ch, &s) in out.iter_mut().zip(f) {
                ch[base + i] = s;
            }
        };
        if self.offline.is_some() {
            self.offline_render(clock, range, positions, emit);
        } else {
            self.live_render(clock, range, positions, &mut Out::Frames(emit));
        }
    }

    /// A block longer than its positions, through a stack array in pieces
    /// of [`UNPREPARED_FRAMES`]: only a voice driven by hand without a
    /// `prepare` (no positions) or past the `max_block` it was prepared for
    /// reaches this, so the prepared render never initialises the array.
    #[cold]
    #[inline(never)]
    fn render_unprepared(&mut self, clock: &BlockClock<'_>, frames: usize, out: &mut [&mut [f32]]) {
        let mut positions = [None; UNPREPARED_FRAMES];
        let mut from = 0;
        while from < frames {
            let to = (from + UNPREPARED_FRAMES).min(frames);
            self.render_piece(clock, from..to, &mut positions, out);
            from = to;
        }
    }
}

impl DiskVoice {
    /// This voice reading the project's tempo map through `tempo`
    /// ([`TransportSettings::tempo_reader`](tutti_transport::TransportSettings::tempo_reader)).
    /// Over a map whose tempo changes, how long a placed voice has played is
    /// the map's seconds between its start and the playhead, not the beats
    /// over the tempo of the moment; see
    /// [`VoicePool::with_tempo`](super::VoicePool::with_tempo). It matters
    /// only when the voice is a graph node of its own (a voice in a pool or a `VoiceNode` reads its owner's). A fork reads the same map.
    #[must_use]
    pub fn with_tempo(mut self, tempo: TempoReader) -> Self {
        self.tempo = Some(tempo);
        self
    }
}

impl Node for DiskVoice {
    /// No inputs, the file's width out; a generator, never skipped.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.inner.channels())
            .with_tail(Tail::Unbounded)
            // A `DiskSource`'s read, placed on the transport as a memory
            // voice is: `Moderate` for the same reason.
            .with_cost(tutti_graph::Cost::Moderate)
    }

    /// The render rate, and the block's position scratch sized from
    /// `max_block` (a re-prepare at another size resizes it).
    fn prepare(&mut self, p: &Prepare) {
        self.set_render_rate(p.sample_rate());
        let frames = p.max_block().get();
        self.positions.clear();
        self.positions.resize(frames, None);
        self.positions.shrink_to_fit();
    }

    /// The block in one piece through the positions `prepare` sized; a
    /// voice driven by hand unprepared (or past its `max_block`) through
    /// `render_unprepared`'s stack pieces.
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        // Lent for the block and given back after it: the render borrows the
        // whole voice.
        let mut reader = self.tempo.take();
        let map = reader.as_mut().map(TempoReader::lend);
        let clock = self.clock.observe_with(cx.env, map.as_deref());
        let frames = io.frames();
        let (_, mut outs) = io.split();
        let n = self.width().min(outs.len()).min(MAX_SAMPLER_CHANNELS);
        for c in n..outs.len() {
            outs.get(c).fill(0.0);
        }
        let mut refs: [&mut [f32]; MAX_SAMPLER_CHANNELS] =
            std::array::from_fn(|_| Default::default());
        for (slot, ch) in refs.iter_mut().zip(outs.iter_mut()) {
            *slot = ch;
        }
        if frames <= self.positions.len() {
            let mut positions = std::mem::take(&mut self.positions);
            self.render_piece(&clock, 0..frames, &mut positions, &mut refs[..n]);
            self.positions = positions;
        } else {
            self.render_unprepared(&clock, frames, &mut refs[..n]);
        }
        drop(map);
        self.tempo = reader;
        Status::Modified
    }

    /// The read's jump state forgotten, the transport forgotten.
    fn reset(&mut self) {
        self.flush();
        self.clock.reset();
    }
}

/// The handle a host keeps on a [`DiskVoice`] inserted as a node of its
/// own: the stream's playback controls (its cells, which the voice reads once
/// per block). Control thread. The stream's direction and loop are not here:
/// they are the streamer's, set through [`Stream`](crate::Stream).
#[derive(Clone)]
pub struct DiskVoiceControls {
    state: Arc<Cells>,
}

impl std::fmt::Debug for DiskVoiceControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskVoiceControls")
            .field("gain", &self.state.gain())
            .finish_non_exhaustive()
    }
}

impl DiskVoiceControls {
    /// Publishes a new output gain; a NaN or infinite gain is silence.
    pub fn set_gain(&self, gain: Amplitude) {
        self.state.set_gain(gain);
    }

    /// The current output gain.
    pub fn gain(&self) -> Amplitude {
        self.state.gain()
    }

    /// Sets the playback speed (varispeed).
    pub fn set_speed(&self, speed: PlaybackRate) {
        self.state.set_speed(speed);
    }
}

/// A disk voice's fork: [`DiskVoice::fork_copy`] of a template that shares
/// the stream's file, status and cells with the live voice (never its ring
/// reader), taken when the fork is.
struct DiskVoiceFork(DiskVoice);

impl ForkSource for DiskVoiceFork {
    fn fork(&self, mode: ForkMode) -> Result<Forked, ForkCause> {
        if matches!(mode, ForkMode::Live) {
            return Err(ForkCause::new(LiveDiskFork));
        }
        let fork = self.0.fork_copy();
        let health = fork.fault().map(VoiceHealth);
        let forked = Forked::new(Box::new(fork));
        Ok(match health {
            Some(h) => forked.with_health(Arc::new(h)),
            None => forked,
        })
    }
}

impl IntoNode for DiskVoice {
    type Controls = DiskVoiceControls;

    fn into_parts(self) -> NodeParts<DiskVoiceControls> {
        let controls = DiskVoiceControls {
            state: Arc::clone(&self.shared_state),
        };
        let fork = DiskVoiceFork(self.clone());
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

#[cfg(test)]
mod live_loop;

// Whether a test has switched the silent-block skip off (see
// `DiskVoice::render_lanes`), so it can compare against the unskipped
// render. The audio thread's code reads a constant `false` outside tests.
#[cfg(test)]
thread_local! {
    pub(crate) static NO_SKIP: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn force_no_skip() -> bool {
    NO_SKIP.with(std::cell::Cell::get)
}

#[cfg(not(test))]
#[inline(always)]
fn force_no_skip() -> bool {
    false
}

#[cfg(test)]
mod tests;
