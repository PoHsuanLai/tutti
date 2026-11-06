//! Disk streaming sample playback, fed by the butler thread.
//!
//! Two types, one tier: [`DiskSource`] is the graph node that reads the
//! butler's ring free-running (its own position, stepping by the read rate),
//! and [`DiskVoice`] is a timeline clip over the same stream: it places its
//! read on the transport exactly as the memory tier's placed read does
//! (`interp::place`, the same gate and step, from the block's `Env`) and
//! reads the ring at that position. Both read through
//! `LiveRead`: the ring indexed by position, the memory tier's tap layout, the
//! one kernel — so a clip plays what the same clip in memory plays at the same
//! clock frame, and a jump is a crossfade from where it was, not a flush.
//!
//! Every count crossing the ring is in frames. Nothing on the `process`
//! path allocates, locks, or blocks — but a fork's: it reads its file
//! (`offline_read`), which is why a disk voice forks only for an offline
//! render.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
use super::interp::{Gate, past_window, place};
use super::live_read::{LiveRead, Out};
use super::memory_source::VoiceWindow;
use super::offline_read::OfflineRead;
use super::types::Direction;
use crate::butler::control::StreamOrigin;
use crate::butler::{RegionId, RtState, SharedReader};
use crate::lanes::{Gather, Lanes, UNPREPARED_FRAMES};

/// Frames a free-running block's positions are computed for at a time:
/// `process` renders a longer block in pieces this long, so the positions
/// live in a fixed array.
const BLOCK_FRAMES: usize = 256;

/// Disk streaming sampler, free-running: the streaming tier's bare reader.
///
/// Reads the butler's ring at its own position — the stream's origin (its
/// offset), stepped by the read rate (`RtState::read_rate`: varispeed,
/// conversion, stretch), less the channel's PDC preroll — and follows a
/// `Command::Seek` the butler relays (`Ring::seek_request`). The butler fills
/// the ring ahead of where it reads (`Ring::play`). A [`DiskVoice`] wraps one
/// for its width and its ring, and reads by the clock instead.
///
/// The audio thread holds the ring through a `SharedReader` (an `Arc`) and
/// the ring's one `PosReader`, which this source owns and a clone does not
/// get (`LiveRead`'s module docs): every access is an atomic.
///
/// As a graph node it refuses every fork
/// ([`ForkError::NotForkable`](tutti_graph::ForkError::NotForkable)): a copy
/// cannot read the live ring, and it knows neither where on the timeline it
/// plays nor its file, so the only copy it could give is silence — which an
/// export would write as if it were the graph's. A [`DiskVoice`] forks.
pub struct DiskSource {
    /// Boxed: a voice in a pool should not carry the reader's jump state
    /// inline (it is built on the control thread, where the box is).
    read: Box<LiveRead>,
    playing: AtomicBool,

    sample_rate: SampleRate,

    /// Shared state for cross-thread communication (speed, direction, gain).
    shared_state: Option<Arc<RtState>>,

    /// The free-running position, file frames before the preroll; `None`
    /// until the first block takes the ring's origin.
    position: Option<f64>,
    /// The last `Command::Seek` epoch applied.
    applied_seek: u64,

    /// Output width — the declaration.
    channels: ChannelLayout,
    /// `channels.count()`, cached.
    stride: usize,
}

// Hand-rolled: `read` holds the ring and `shared_state` an `Arc<RtState>`.
impl std::fmt::Debug for DiskSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskSource")
            .field("playing", &self.playing.load(Ordering::Relaxed))
            .field("sample_rate", &self.sample_rate)
            .field("has_shared_state", &self.shared_state.is_some())
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

impl Clone for DiskSource {
    fn clone(&self) -> Self {
        Self {
            read: self.read.clone(),
            playing: AtomicBool::new(self.playing.load(Ordering::Relaxed)),
            sample_rate: self.sample_rate,
            shared_state: self.shared_state.clone(),
            position: self.position,
            applied_seek: self.applied_seek,
            channels: self.channels,
            stride: self.stride,
        }
    }
}

impl DiskSource {
    /// A source over `consumer`, taking the ring's reader (test rings; a
    /// taken one leaves this source silent).
    #[cfg(test)]
    pub(crate) fn new(consumer: SharedReader, shared_state: Arc<RtState>) -> Self {
        let reader = consumer.take_reader(false).ok();
        Self::with_reader(consumer, reader, shared_state)
    }

    /// A source over `consumer` through its one `reader`. Width comes from
    /// the ring (narrowed to [`MAX_SAMPLER_CHANNELS`], the widest frame the
    /// read path stacks): the file's own layout.
    pub(crate) fn with_reader(
        consumer: SharedReader,
        reader: Option<tutti_types::PosReader>,
        shared_state: Arc<RtState>,
    ) -> Self {
        let stride = (consumer.channels().count() as usize).clamp(1, MAX_SAMPLER_CHANNELS);
        let channels = ChannelLayout::from(stride);
        Self {
            read: Box::new(LiveRead::new(consumer, reader, stride)),
            playing: AtomicBool::new(true),
            sample_rate: SampleRate::SR_44K1,
            shared_state: Some(shared_state),
            position: None,
            // Seek epochs count from 0: a seek before this reader existed is
            // applied at its first block.
            applied_seek: 0,
            channels,
            stride,
        }
    }

    /// Starts reading. One relaxed atomic store, safe from the audio thread.
    pub fn play(&self) {
        self.playing.store(true, Ordering::Relaxed);
    }

    /// Emits silence and stops reading. The butler keeps its window where the
    /// reader last played.
    pub fn stop(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    /// Whether frames are being read. `process` checks this before touching
    /// the ring.
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Publishes a new output gain, to the shared `RtState` (see `tutti_nodes`'
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

    /// Render `size` frames free-running, frame `i` handed to `emit`.
    fn render(&mut self, size: usize, mut emit: impl FnMut(usize, &[f32])) {
        // Borrowed beside the reader, not cloned: `self` holds a strong
        // reference, so a clone only adds two atomic refcount updates.
        let Some(state) = self.shared_state.as_deref() else {
            let silent = [0.0f32; MAX_SAMPLER_CHANNELS];
            (0..size).for_each(|i| emit(i, &silent[..self.stride]));
            return;
        };
        let ring = self.read.ring();
        let (epoch, target) = ring.seek_request();
        if epoch != self.applied_seek {
            self.applied_seek = epoch;
            self.position = Some(target as f64);
        }
        let mut position = self.position.unwrap_or(ring.origin() as f64);
        let preroll = ring.preroll() as f64;
        let rate = state.read_rate().get();
        let gain = state.gain().get();
        let mut positions = [None; BLOCK_FRAMES];
        let mut done = 0;
        while done < size {
            let n = (size - done).min(BLOCK_FRAMES);
            for (k, p) in positions[..n].iter_mut().enumerate() {
                *p = Some(SamplePosition(
                    (position + rate * k as f64 - preroll).max(0.0),
                ));
            }
            // A relayed seek is a new segment, even to where it stands.
            self.read.render(
                &positions[..n],
                self.applied_seek,
                rate,
                gain,
                state,
                done,
                &mut Out::Frames(&mut emit),
            );
            position += rate * n as f64;
            done += n;
        }
        self.position = Some(position);
    }
}

impl DiskSource {
    /// Stop reading and let go of the live stream: the ring's reader, and
    /// the control cell. A copy severed so never touches the live stream.
    fn sever(&mut self) {
        self.playing.store(false, Ordering::Relaxed);
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

impl Node for DiskSource {
    /// No inputs, the file's width out (at most [`MAX_SAMPLER_CHANNELS`]); a
    /// generator, never skipped.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.channels)
            .with_tail(Tail::Unbounded)
            // The memory tier's read (`MemorySource`, measured 16–17 ns per
            // frame and channel), through the same kernel, from a ring of
            // atomics: not measured on its own (doc 013, "The cost model and coarsening").
            .with_cost(tutti_graph::Cost::Moderate)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.sample_rate = p.sample_rate();
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let frames = io.frames();
        let (_, mut outs) = io.split();
        if !self.playing.load(Ordering::Relaxed) {
            for ch in outs.iter_mut() {
                ch.fill(0.0);
            }
            // A stopped source holds no refill back.
            self.read.idle();
            return Status::Modified;
        }
        let n = self.width().min(outs.len());
        let mut refs: [&mut [f32]; MAX_SAMPLER_CHANNELS] =
            std::array::from_fn(|_| Default::default());
        for (slot, ch) in refs.iter_mut().zip(outs.iter_mut()) {
            *slot = ch;
        }
        self.render(frames, |i, f| {
            for (c, &s) in f[..n].iter().enumerate() {
                refs[c][i] = s;
            }
        });
        Status::Modified
    }

    /// Stopped, and the read's jump state forgotten.
    fn reset(&mut self) {
        self.playing.store(false, Ordering::Relaxed);
        self.reset_interpolation();
    }
}

impl IntoNode for DiskSource {
    type Controls = ();

    /// No fork source: see the type's docs.
    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(self),
            controls: (),
            fork: None,
        }
    }
}

// ---------------------------------------------------------------------------
// DiskVoice — a timeline clip over a stream.
//
// Per frame: the placement (`interp::place`, the memory tier's) gives the
// position the transport puts the playhead at inside the window, `None`
// outside it or on a standing transport; the ring is read there. The butler
// fills ahead of where the voice reads; a jump is the reader's own crossfade
// (`LiveRead`).
// ---------------------------------------------------------------------------

/// Wiring for [`DiskVoice::new`]: the window plus the file sample rate.
/// `shared_state` stays a separate wiring arg (it must be the same `RtState`
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
    /// The rate the butler recorded from the file's header when it opened the
    /// stream (`Status::take_disk_voice`), not one recovered from the session
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
/// butler records it (path, rate, loop), to read on demand on the render's
/// thread. It plays the same window of the file on the render's transport
/// (it reads its render's `Env`), read as the memory tier reads it; see
/// `offline_read`. That path blocks on file I/O and is never taken live: a
/// live fork of a disk voice is refused.
pub struct DiskVoice {
    inner: DiskSource,
    /// The playback controls: the butler's cell, shared with `inner`, while
    /// live; a private snapshot of it in a fork.
    shared_state: Arc<RtState>,

    /// Span of timeline this voice occupies.
    window: VoiceWindow,

    /// File sample rate — converts the transport's second-offset into a file
    /// position, matching `MemorySource`'s use of `wave.sample_rate()`.
    file_sample_rate: SampleRate,

    /// The butler stream this voice consumes, as a read-only handle onto the
    /// butler's record of it: what a fork reads its file from. `None` for a
    /// voice not built by [`Status::take_disk_voice`](crate::Status::take_disk_voice)
    /// (a test's bare ring), whose fork then plays silence.
    origin: Option<StreamOrigin>,

    /// `Some` in a fork ([`fork_copy`](Self::fork_copy)): it never touches
    /// the ring or the butler, and plays what this holds instead. Boxed: a
    /// live voice (every voice in a pool) should not carry the pages' room,
    /// and it is built on the control thread, where a fork is taken.
    offline: Option<Box<Offline>>,

    /// The rate `prepare` last gave this voice, `None` until one did: the
    /// rate the step converts to. A fork never told a rate renders nothing
    /// and says so rather than play off pitch; a live voice never told one
    /// steps by the butler's conversion for the session rate.
    sample_rate: Option<SampleRate>,

    /// The transport as this voice reads it when it is a graph node of its
    /// own (a voice in a pool or a `VoiceNode` reads its owner's).
    clock: Clock,

    /// A block's positions when this voice is a graph node of its own,
    /// sized from `Prepare::max_block` in `prepare` (empty until then and in
    /// a clone, so a voice that only ever plays in a pool or a `VoiceNode`,
    /// which read through their own scratch, carries none).
    positions: Vec<Option<SamplePosition>>,
}

/// A forked disk voice's own playback: the file it reads, and where its
/// failures go.
#[derive(Clone, Debug, Default)]
struct Offline {
    /// The file, from the butler's record when the fork was taken. `None`
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
            // Unsized, so a clone stays allocation-free as it was: a copy
            // inserted as a node is prepared, which sizes it.
            positions: Vec::new(),
        }
    }
}

impl DiskVoice {
    /// Places a `DiskSource`'s stream on the timeline.
    ///
    /// Construction (butler stream registration, ring allocation) happens on
    /// the ECS/butler side; this only binds the already-built unit to a
    /// timeline window. `shared_state` must be the same `RtState` the `inner`
    /// unit holds.
    pub fn new(inner: DiskSource, shared_state: Arc<RtState>, config: DiskVoiceConfig) -> Self {
        Self {
            inner,
            shared_state,
            window: config.window,
            file_sample_rate: config.file_sample_rate,
            origin: None,
            offline: None,
            sample_rate: None,
            clock: Clock::new(),
            positions: Vec::new(),
        }
    }

    /// The butler stream this voice reads: its ring's region, which a loop
    /// forwarded for the voice names so the butler ignores it once the
    /// channel carries another stream.
    pub(crate) fn stream(&self) -> RegionId {
        self.inner.read.ring().region_id()
    }

    /// Record which butler stream this voice consumes, so a fork of it can
    /// read the same file (see "A fork reads the file itself").
    pub(crate) fn with_origin(mut self, origin: StreamOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    /// The voice's window on the timeline.
    pub fn window(&self) -> VoiceWindow {
        self.window
    }

    /// A copy of this voice for an offline render, sharing nothing with
    /// it: cut off from the stream (no ring reader, no control cell — a
    /// private snapshot of the controls' values, `RtState::detached`), and
    /// handed the stream's file, as the butler's record of it names it now
    /// (with the loop set on it now), to read itself. A stream that has ended
    /// by now is a latched failure ([`fault`](Self::fault)): the export
    /// fails naming the voice rather than write its silence.
    ///
    /// Takes the stream record's lock: control thread only, as every fork
    /// is.
    pub(crate) fn fork_copy(&self) -> Self {
        let mut copy = self.clone();
        copy.inner.sever();
        copy.shared_state = Arc::new(self.shared_state.detached());
        copy.clock = Clock::new();
        let fault = Arc::new(FaultLatch::default());
        let read = match &self.origin {
            Some(origin) => match origin.describe() {
                Some(file) => Some(OfflineRead::new(file, Arc::clone(&fault))),
                None => {
                    fault.latch(OfflineFault::StreamGone);
                    None
                }
            },
            // A fork of a fork keeps the file it was handed; a voice over a
            // bare ring has none.
            None => self
                .offline
                .as_ref()
                .and_then(|o| o.read.clone())
                .map(|read| read.relatched(Arc::clone(&fault))),
        };
        copy.origin = None;
        copy.offline = Some(Box::new(Offline { read, fault }));
        copy
    }

    /// A fork's failure latch; `None` live.
    pub(crate) fn fault(&self) -> Option<Arc<FaultLatch>> {
        self.offline.as_ref().map(|o| Arc::clone(&o.fault))
    }

    /// Tells the stream how fast a wrapping time-stretcher wants its source.
    ///
    /// `1 / stretch`, or [`ReadRate::UNITY`] when nothing wraps this voice. See
    /// `RtState::read_rate` for why the factor lands there rather than at the
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

    /// Publishes a new output gain. `&self`: the write lands in the shared
    /// `RtState`, so no exclusivity is needed.
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

    /// Sets the playback speed magnitude, in the shared `RtState` (what the
    /// butler's `SetVarispeed` also sets). The next block reads at the
    /// position the new speed gives, crossfaded.
    pub fn set_speed(&mut self, speed: PlaybackRate) {
        self.shared_state.set_speed(speed);
    }

    /// Sets the playback direction, in the shared `RtState`. The butler turns
    /// the ring's mapping on its next cycle (reverse ignores the loop and
    /// mirrors the file, as the memory tier's reverse does).
    pub fn set_direction(&mut self, direction: Direction) {
        self.shared_state.set_direction(direction);
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
    /// never told a rate takes the butler's conversion for the session.
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
    /// the placed positions, less the channel's preroll, read from the ring,
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

    /// The straight positions of live frames `range` in `positions[..len]`,
    /// less the channel's preroll (the ring's); whether any frame has one.
    fn live_place(
        &self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
    ) -> bool {
        let preroll = self.inner.read.ring().preroll() as f64;
        let positions = &mut positions[..range.len()];
        let sounds = place(clock, range, self.gate(), self.step_rate(), positions);
        for p in positions.iter_mut() {
            *p = p.map(|s| SamplePosition((s.get() - preroll).max(0.0)));
        }
        sounds
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
        let gain = self.shared_state.gain().get();
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
        let direction = self.shared_state.direction();
        let gain = self.shared_state.gain().get();
        let told_rate = self.sample_rate.is_some();
        place(clock, range.clone(), gate, rate, &mut positions[..frames]);
        let past = past_window(clock, range, gate);
        let Some(offline) = self.offline.as_mut() else {
            (0..frames).for_each(|i| emit(i, &silent[..w]));
            return;
        };
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
                Some(read) => read.read_into(*pos, direction, &mut frame[..w]),
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
        self.inner.sample_rate = sample_rate;
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
        let clock = self.clock.observe(cx.env);
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
        Status::Modified
    }

    /// The read's jump state forgotten, the transport forgotten.
    fn reset(&mut self) {
        self.flush();
        self.clock.reset();
    }
}

/// The handle a host keeps on a [`DiskVoice`] inserted as a node of its
/// own: the stream's playback controls (the butler's control cell, which
/// the voice reads once per block). Control thread.
#[derive(Clone)]
pub struct DiskVoiceControls {
    state: Arc<RtState>,
}

impl std::fmt::Debug for DiskVoiceControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskVoiceControls")
            .field("gain", &self.state.gain())
            .finish_non_exhaustive()
    }
}

impl DiskVoiceControls {
    /// Publishes a new output gain.
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

    /// Sets the playback direction.
    pub fn set_direction(&self, direction: Direction) {
        self.state.set_direction(direction);
    }
}

/// A disk voice's fork: [`DiskVoice::fork_copy`] of a template that shares
/// the stream's record and control cell with the live voice (never its ring
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
