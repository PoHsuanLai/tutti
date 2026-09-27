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
//! Every count crossing the ring is in **frames**. Nothing on the `process`
//! path allocates, locks, or blocks — but a **fork**'s: it reads its file
//! (`offline_read`), which is why a disk voice forks only for an offline
//! render.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::lanes::LANE_FRAMES;
use crate::MAX_SAMPLER_CHANNELS;
use tutti_core::{
    Amplitude, Beat, BeatDuration, ChannelLayout, PlaybackRate, ReadRate, SampleRate, SrcRatio,
    Tail,
};
use tutti_graph::{
    Cx, ForkCause, ForkFaultKind, ForkHealth, ForkMode, ForkSource, Forked, IntoNode, Io, Node,
    NodeParts, Prepare, Shape, Status,
};

use super::clock::{BlockClock, Clock};
use super::fault::FaultLatch;
use super::interp::{past_window, place, Gate};
use super::live_read::LiveRead;
use super::memory_source::VoiceWindow;
use super::offline_read::OfflineRead;
use super::types::Direction;
use crate::butler::control::StreamOrigin;
use crate::butler::{RtState, SharedReader};
use crate::lanes::Lanes;

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
/// As a graph node it **refuses every fork**
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
        reader: Option<tutti_core::PosReader>,
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
        let Some(state) = self.shared_state.clone() else {
            let silent = [0.0f32; MAX_SAMPLER_CHANNELS];
            (0..size).for_each(|i| emit(i, &silent[..self.stride]));
            return;
        };
        let ring = Arc::clone(self.read.ring());
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
                *p = Some((position + rate * k as f64 - preroll).max(0.0));
            }
            // A relayed seek is a new segment, even to where it stands.
            self.read.render(
                &positions[..n],
                self.applied_seek,
                rate,
                gain,
                &state,
                |i, f| emit(done + i, f),
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
        Shape::audio(ChannelLayout::EMPTY, self.channels).with_tail(Tail::Unbounded)
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
#[derive(Clone, Debug)]
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
/// (its `Env`: nothing to rebind), read as the memory tier reads it; see
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
        }
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

    /// A copy of this voice for an **offline** render, sharing nothing with
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
    /// **Callers must publish unity when they stop stretching.** The rate
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

    /// Render block frames `range` (at most one lane) into frames
    /// `0..range.len()` of the first `n` lanes, every frame written: the
    /// voice's own channels, then silence on any lane past them. The live
    /// read claims the ring once per run of the block (a transport change or
    /// a loop wrap starts a run); a fork reads its file frame by frame. The
    /// block read of a slot holding this voice (`PlaybackSlot::render_lanes`),
    /// and of the voice as a node.
    pub(crate) fn render_lanes(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        lanes: &mut Lanes,
        n: usize,
    ) {
        let w = self.width().min(n);
        let frames = range.len();
        lanes.clear(n, frames);
        if self.offline.is_some() {
            self.offline_render(clock, range, |i, f| lanes.put(i, &f[..w]));
            return;
        }
        self.live_render(clock, range, |i, f| lanes.put(i, &f[..w]));
    }

    /// Render live frames `range`, frame `i - range.start` handed to `emit`:
    /// the placed positions, less the channel's preroll, read from the ring,
    /// one ring read per run (its generation the read's segment, so a jump
    /// is crossfaded).
    fn live_render(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        mut emit: impl FnMut(usize, &[f32]),
    ) {
        let rate = self.step_rate();
        let preroll = self.inner.read.ring().preroll() as f64;
        let gain = self.shared_state.gain().get();
        let state = Arc::clone(&self.shared_state);
        let gate = self.gate();
        let base = range.start;
        let mut placed = [None; LANE_FRAMES];
        let mut positions = [None; LANE_FRAMES];
        debug_assert!(range.len() <= LANE_FRAMES, "a piece longer than a lane");
        for run in clock.runs_in(range) {
            let (a, b) = (run.start, run.end);
            let n = b - a;
            place(clock, a..b, gate, rate, &mut placed[..n]);
            for (p, s) in positions[..n].iter_mut().zip(&placed[..n]) {
                *p = s.map(|s| (s.get() - preroll).max(0.0));
            }
            self.inner.read.render(
                &positions[..n],
                run.generation,
                rate.get(),
                gain,
                &state,
                |i, f| emit(a - base + i, f),
            );
        }
    }

    /// A fork's frames `range`, frame `i - range.start` handed to `emit`.
    ///
    /// The placement is the live one; where the live voice reads the ring,
    /// this reads the file at the placed position. Once the playhead is past
    /// the window's end, the file is closed: a render holding many voices
    /// keeps a file open per voice sounding.
    fn offline_render(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
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
        let mut positions = [None; LANE_FRAMES];
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
        if past {
            if let Some(read) = offline.read.as_mut() {
                read.close();
            }
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
}

impl Node for DiskVoice {
    /// No inputs, the file's width out; a generator, never skipped.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.inner.channels()).with_tail(Tail::Unbounded)
    }

    fn prepare(&mut self, p: &Prepare) {
        self.set_render_rate(p.sample_rate());
    }

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
        let mut from = 0;
        while from < frames {
            let to = (from + LANE_FRAMES).min(frames);
            let emit = |i: usize, f: &[f32]| {
                for (c, &s) in f[..n].iter().enumerate() {
                    refs[c][from + i] = s;
                }
            };
            if self.offline.is_some() {
                self.offline_render(&clock, from..to, emit);
            } else {
                self.live_render(&clock, from..to, emit);
            }
            from = to;
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
    fn fork(&self, mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::{RegionBuffer, RegionId};
    use crate::testing::{block, MockTransport};
    use std::path::PathBuf;
    use tutti_core::{Bpm, SamplePosition, Samples};
    use tutti_graph::contract::Direct;

    /// The rate the tests' blocks run at.
    const SR: f64 = 44_100.0;

    /// A stopped transport: a free-running source reads none.
    fn stopped() -> Arc<MockTransport> {
        MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0))
    }

    /// One block of `n` frames of a free-running `unit`.
    fn free(unit: &mut DiskSource, n: usize) -> Vec<Vec<f32>> {
        block(unit, &stopped(), SR, n)
    }

    /// One block of `n` frames of `voice` under `t`, at `rate`.
    fn under(voice: &mut DiskVoice, t: &MockTransport, rate: f64, n: usize) -> Vec<Vec<f32>> {
        block(voice, t, rate, n)
    }

    /// Tests still author stereo pairs for readability; flatten them at the one
    /// boundary rather than rewriting every fixture. The frames land at
    /// straight positions `0..`, where a free-running source starts.
    fn make_reader_with_samples(samples: &[(f32, f32)]) -> SharedReader {
        make_reader_at(samples, 0)
    }

    /// [`make_reader_with_samples`], the frames landing at straight positions
    /// `start..` (where a placed voice reads them).
    fn make_reader_at(samples: &[(f32, f32)], start: u64) -> SharedReader {
        let (mut writer, reader) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), samples.len() + 64, 2usize);
        writer.reset(start);
        writer.set_play(start);
        let flat: Vec<f32> = samples.iter().flat_map(|&(l, r)| [l, r]).collect();
        writer.push_interleaved(&flat);
        reader
    }

    fn make_unit(samples: &[(f32, f32)]) -> (DiskSource, Arc<RtState>) {
        let reader = make_reader_with_samples(samples);
        let state = Arc::new(RtState::new());
        let unit = DiskSource::new(reader, Arc::clone(&state));
        (unit, state)
    }

    /// **At unity rate a free-running source reads one file frame per output
    /// frame**, exactly: output frame `i` is the ring's frame `i`, and after 8
    /// blocks of 64 it stands on frame 511.
    ///
    /// A reader that popped four frames of interpolation head-room per block
    /// and dropped them would run 68/64 fast: every streamed file a
    /// quarter-tone sharp with its level and waveform intact. Reading by
    /// position has no fetch step to get wrong, but the step is still the one
    /// quantity that decides pitch, so it stays pinned.
    ///
    /// Mutation (run): the position stepped by `n + 4` frames a block → frame
    /// 64 reads 68 → fails.
    #[test]
    fn process_reads_one_file_frame_per_output_frame_at_unity_rate() {
        const BLOCK: usize = 64;
        const BLOCKS: usize = 8;
        let samples: Vec<(f32, f32)> = (0..2048)
            .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
            .collect();
        let (mut unit, _state) = make_unit(&samples);
        unit.play();

        for block in 0..BLOCKS {
            let out = free(&mut unit, BLOCK);
            for (i, &got) in out[0].iter().enumerate() {
                let frame = block * BLOCK + i;
                assert_eq!(got, samples[frame].0, "output frame {frame}");
            }
        }
        assert_eq!(unit.read.read_to(), (BLOCK * BLOCKS - 1) as f64);
    }

    // --- DiskVoice: placement gate ---

    /// A voice at 44.1 kHz over a ring holding `samples` from straight
    /// position 0.
    fn make_clip_reader(
        samples: &[(f32, f32)],
        start_beat: Beat,
        duration: Option<BeatDuration>,
    ) -> DiskVoice {
        make_clip_reader_at(samples, 0, start_beat, duration)
    }

    /// [`make_clip_reader`] over a ring holding `samples` from `at` on.
    fn make_clip_reader_at(
        samples: &[(f32, f32)],
        at: u64,
        start_beat: Beat,
        duration: Option<BeatDuration>,
    ) -> DiskVoice {
        let state = Arc::new(RtState::new());
        let inner = DiskSource::new(make_reader_at(samples, at), Arc::clone(&state));
        DiskVoice::new(
            inner,
            state,
            DiskVoiceConfig {
                window: VoiceWindow {
                    start: start_beat,
                    duration,
                },
                file_sample_rate: SampleRate(44100.0),
            },
        )
    }

    /// A 48 kHz file in a 44.1 kHz session must read where the clock puts
    /// the playhead in the *file's* frames — `src_ratio` applied exactly once.
    ///
    /// `file_sample_rate` is the file's rate (`ports.rs` hands over the one
    /// the butler recorded), so the gate already measures in file frames;
    /// composing the conversion into the gate as well applied it twice. Every
    /// other streaming fixture hardcodes 44100 against a default `src_ratio` of
    /// 1.0 — the one value that makes a double-multiply invisible. Observed
    /// where the reader says it plays (`Ring::play`, what the butler fills).
    ///
    /// Mutation (run): the gate's rate composing the conversion
    /// (`window_rate` via `read_rate`) → ~104 490 → fails.
    #[test]
    fn placement_gate_applies_src_ratio_exactly_once() {
        let samples: Vec<_> = (1..64).map(|i| (i as f32, i as f32)).collect();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));

        let (inner, state) = make_unit(&samples);
        let ring = Arc::clone(inner.read.ring());
        let src = 48_000.0 / 44_100.0;
        state.set_src_ratio(tutti_core::SrcRatio::new(src as f32));

        let mut reader = DiskVoice::new(
            inner,
            state,
            DiskVoiceConfig {
                window: VoiceWindow {
                    start: Beat::new(0.0),
                    duration: None,
                },
                // The file's rate, as `ports.rs` hands it over: session × src.
                file_sample_rate: SampleRate(44_100.0 * src),
            },
        );

        // Two seconds in at 120 BPM = beat 4.0.
        transport.set_beat(Beat::new(4.0));
        reader.set_render_rate(SampleRate(44_100.0));
        under(&mut reader, &transport, SR, 1);
        let offset = ring.play() as f64;

        // 2 s of a 48 kHz file is 96000 file samples — src_ratio applied once.
        let expected = 2.0 * 48_000.0;
        assert!(
            (offset - expected).abs() < 1.0,
            "expected ~{expected} file samples, got {offset} \
             (a double-applied src_ratio gives ~{})",
            expected * src
        );
    }

    /// **A varispeed change moves where the voice reads, without any beat
    /// discontinuity.**
    ///
    /// A cursor watching beats is blind to it: at beat 20 of a 120 BPM
    /// timeline, 1.0x -> 2.0x relocates the file position by 441 000 frames
    /// while the transport reports the same beat, at the same tempo, still
    /// rolling. The next block reads where the gate puts the playhead at the
    /// new speed and tells the butler so (`Ring::play`), which fills there.
    ///
    /// Mutation (run): the gate's rate ignoring varispeed → the voice reads
    /// on near 441 000 → fails.
    #[test]
    fn a_varispeed_change_moves_the_read_without_any_beat_discontinuity() {
        let samples: Vec<_> = (1..4096)
            .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
            .collect();
        let transport = MockTransport::rolling(Beat::new(20.0), Bpm::new(120.0));
        let mut reader = make_clip_reader(&samples, Beat::new(0.0), None);
        let ring = Arc::clone(reader.inner.read.ring());

        under(&mut reader, &transport, SR, 1);
        let before = ring.play();
        assert_eq!(before, 441_000, "10 s of a 44.1 kHz file");

        let beat_before = transport.beat();
        reader.set_speed(PlaybackRate::new(2.0));
        under(&mut reader, &transport, SR, 1);
        assert_eq!(
            transport.beat(),
            beat_before,
            "the playhead must not have moved; otherwise this proves nothing"
        );
        assert_eq!(ring.play(), 882_000, "at 2x beat 20 is 20 s in");
        // One frame on: two file frames on.
        transport.advance(1, SR);
        under(&mut reader, &transport, SR, 1);
        assert_eq!(ring.play(), 882_002, "a frame later, two file frames on");
    }

    /// **A fork past its window closes its file**, and one inside it holds it
    /// open: a render of many voices keeps a file open per voice sounding.
    /// Mutation (run): the close removed from `offline_render`'s past-the-window
    /// branch → still open → fails.
    #[test]
    fn a_fork_closes_its_file_once_past_its_window() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("ramp.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&path, spec).expect("writes");
        for i in 0..60_000 {
            w.write_sample(i as f32 * 1e-5).expect("writes");
            w.write_sample(i as f32 * 1e-5).expect("writes");
        }
        w.finalize().expect("writes");
        let mut streamer =
            crate::DiskStreamer::manual(SampleRate(48_000.0), Default::default()).expect("builds");
        streamer
            .commands()
            .send(crate::Command::Stream {
                channel_index: 0,
                file_path: path,
                offset: SamplePosition(0.0),
            })
            .expect("the butler is alive");
        let _ = streamer.step_until_settled(1_000);
        // Beats [0, 1): 24 000 frames at 120 BPM.
        let voice = streamer
            .status()
            .take_disk_voice(0, Beat::new(0.0), Some(BeatDuration::new(1.0)))
            .expect("the link is installed");

        let render = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let mut copy = voice.fork_copy();
        copy.set_render_rate(SampleRate(48_000.0));
        let is_open = |copy: &DiskVoice| {
            copy.offline
                .as_ref()
                .and_then(|offline| offline.read.as_ref())
                .is_some_and(OfflineRead::is_open)
        };
        let mut played = 0;
        while played < 30_000 {
            under(&mut copy, &render, 48_000.0, 64);
            played += 64;
            render.advance(64, 48_000.0);
            if played == 1_024 {
                assert!(is_open(&copy), "the file is open while the clip plays");
            }
        }
        assert!(!is_open(&copy), "the file is still open past the window");
    }

    /// **A stopped clock silences a fork mid-clip**, in a 64-frame block and
    /// in one-frame ones: the clock stops where it stands (its beat does not
    /// move), and the read must not run on as if it still rolled. The
    /// placement is the memory tier's too (`memory_source`'s
    /// `a_stopped_clock_silences_a_placed_read`).
    ///
    /// Mutation (run): `place` ignoring `run.rolling()` → the read plays on
    /// through the stop → fails.
    #[test]
    fn a_stopped_clock_silences_a_fork() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let path = dir.path().join("ramp.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut w = hound::WavWriter::create(&path, spec).expect("writes");
        for i in 0..20_000 {
            w.write_sample((i + 1) as f32 * 1e-5).expect("writes");
            w.write_sample((i + 1) as f32 * 1e-5).expect("writes");
        }
        w.finalize().expect("writes");
        let mut streamer =
            crate::DiskStreamer::manual(SampleRate(48_000.0), Default::default()).expect("builds");
        streamer
            .commands()
            .send(crate::Command::Stream {
                channel_index: 0,
                file_path: path,
                offset: SamplePosition(0.0),
            })
            .expect("the butler is alive");
        let _ = streamer.step_until_settled(1_000);
        let voice = streamer
            .status()
            .take_disk_voice(0, Beat::new(0.0), None)
            .expect("the link is installed");

        for single in [false, true] {
            let mut copy = voice.fork_copy();
            copy.set_render_rate(SampleRate(48_000.0));
            let clock = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
            let blk = |copy: &mut DiskVoice| -> Vec<f32> {
                if single {
                    (0..64)
                        .map(|_| under(copy, &clock, 48_000.0, 1)[0][0])
                        .collect()
                } else {
                    under(copy, &clock, 48_000.0, 64).swap_remove(0)
                }
            };
            assert!(
                blk(&mut copy).iter().all(|&s| s != 0.0),
                "single frames {single}: rolling, the clip plays"
            );
            clock.set_rolling(false);
            assert!(
                blk(&mut copy).iter().all(|&s| s == 0.0),
                "single frames {single}: stopped, the fork plays on"
            );
        }
    }

    /// **A severed source lets go of the live reader.** A clone never holds
    /// it (the reader is the live source's alone), so the one
    /// way a severed copy could still speak for the live ring is a source
    /// severed in place (what a disk voice's fork does to its copy's source).
    /// Here the live source claims a block, is severed, and, stopped, renders
    /// again: the ranges the live block claimed must still stand, and a clone
    /// of it renders nothing into them either.
    ///
    /// Mutation (run): `sever` not letting go of the reader (`read.sever()`
    /// removed) → the severed source's stopped block idles the reader and
    /// clears its ranges → fails.
    #[test]
    fn a_severed_copy_holds_no_live_reader() {
        let (mut writer, ring) =
            RegionBuffer::with_capacity(RegionId(1), PathBuf::new(), 4_096, 2usize);
        writer.push_interleaved(&[0.5; 2 * 1_000]);
        let mut live = DiskSource::new(ring, Arc::new(RtState::new()));
        free(&mut live, 64);
        let claimed = writer.in_flight_end();
        assert!(claimed > 0, "the live block claimed nothing");
        let mut clone = live.clone();
        clone.play();
        free(&mut clone, 64);
        assert_eq!(
            writer.in_flight_end(),
            claimed,
            "a clone spoke for the live reader"
        );
        live.sever();
        free(&mut live, 64);
        assert_eq!(
            writer.in_flight_end(),
            claimed,
            "a severed source spoke for the live reader"
        );
    }

    /// **A fork of a disk voice never touches the live stream**: rendered
    /// inside its window on the render's transport, it tells the live butler
    /// no position (so the live window stays where the live voice reads), and
    /// its controls are its own. What it plays instead is
    /// `tests/offline_disk_voice.rs`'s. The node's fork source forks it for an
    /// offline render and **refuses a live one**, which would read the file
    /// on the audio thread.
    ///
    /// Mutation (run): `fork_copy` keeping the live `shared_state` → the
    /// copy's gain write lands on the live cell → fails. Mutation (run):
    /// `fork_copy` returning the copy on the live path over the live cell
    /// (before it detaches the cell and hands the copy its file) → fails.
    /// Mutation (run): `DiskVoiceFork` not refusing `ForkMode::Live` →
    /// fails. (Not caught: `fork_copy` not severing its source alone — a
    /// clone of a source never holds the live reader, so severing it again
    /// changes nothing; `a_severed_copy_holds_no_live_reader` pins `sever`.)
    #[test]
    fn a_fork_never_touches_the_live_stream() {
        let samples: Vec<_> = (1..4096).map(|i| (i as f32, i as f32)).collect();
        let live = make_clip_reader(&samples, Beat::new(0.0), None);
        let ring = Arc::clone(live.inner.read.ring());
        let (play_before, window_before) = (ring.play(), ring.window());

        let render = MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
        let mut copy = live.fork_copy();
        for _ in 0..16 {
            under(&mut copy, &render, SR, 64);
            render.advance(64, SR);
        }
        copy.set_gain(Amplitude::new(0.25));

        assert_eq!(ring.play(), play_before, "a copy moved the live read");
        assert_eq!(ring.window(), window_before, "a copy touched the live ring");
        assert_eq!(
            live.gain(),
            Amplitude::new(1.0),
            "a copy moved the live gain"
        );

        let clock = Arc::new(tutti_core::transport::OfflineTimeline::new(
            &tutti_core::transport::OfflineTimelineConfig {
                start_beat: Beat::new(0.0),
                tempo: Bpm::new(120.0),
                sample_rate: SampleRate(48_000.0),
                loop_range: None,
            },
        ));
        let ctx = tutti_core::transport::OfflineTransport::new(clock);
        let source = DiskVoiceFork(live.clone());
        assert!(source.fork(ForkMode::Offline(&ctx)).is_ok());
        let refused = source
            .fork(ForkMode::Live)
            .err()
            .expect("a live fork is refused");
        assert!(refused.downcast_ref::<LiveDiskFork>().is_some());
    }

    #[test]
    fn clip_reader_silent_outside_window_audible_inside() {
        // Voice window: beats [4, 8). Non-zero ramp in the ring where beat 5
        // reads (half a second in: 22 050 frames).
        let samples: Vec<_> = (1..64).map(|i| (i as f32, i as f32)).collect();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let mut reader = make_clip_reader_at(
            &samples,
            22_046,
            Beat::new(4.0),
            Some(BeatDuration::new(4.0)),
        );

        // Before the window (beat 0): silence, ring untouched.
        let out = under(&mut reader, &transport, SR, 8);
        assert!(
            out.iter().flatten().all(|&s| s == 0.0),
            "before window → silence"
        );

        // Inside the window (beat 5): reads the ring, produces audio.
        transport.set_beat(Beat::new(5.0));
        let out = under(&mut reader, &transport, SR, 8);
        assert!(
            out.iter().flatten().any(|&s| s != 0.0),
            "inside window → audible"
        );

        // Past the window (beat 9): silent again.
        transport.set_beat(Beat::new(9.0));
        let out = under(&mut reader, &transport, SR, 1);
        assert_eq!([out[0][0], out[1][0]], [0.0, 0.0], "after window → silence");
    }

    #[test]
    fn clip_reader_stopped_transport_is_silent() {
        let samples: Vec<_> = (1..32).map(|i| (i as f32, i as f32)).collect();
        let transport = MockTransport::stopped(Beat::new(5.0), Bpm::new(120.0)); // inside window but stopped
        let mut reader = make_clip_reader(&samples, Beat::new(4.0), Some(BeatDuration::new(4.0)));

        let out = under(&mut reader, &transport, SR, 1);
        assert_eq!(
            [out[0][0], out[1][0]],
            [0.0, 0.0],
            "stopped transport → silence"
        );
    }

    // --- DiskVoice: RT no-alloc (steady-state process inside window) ---
    //
    // The integration-test gate `tests/rt_no_alloc.rs` cannot reach the
    // crate-private ring / `RtState` needed to build a streaming reader, so the streaming-variant no-alloc guard lives here, in-crate, with
    // a module-local `AllocDisabler`. The disabler only aborts inside an
    // `assert_no_alloc` region; every other unit test allocates normally.

    #[global_allocator]
    static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

    /// A clip reader inside its window, driven by hand (`Direct`) at 48 kHz
    /// in `frames`-frame blocks, warmed up: the enter-window edge settled and
    /// the interpolation history primed.
    fn warm_clip_reader(transport: &MockTransport, frames: usize) -> Direct<DiskVoice> {
        let samples: Vec<_> = (1..2048)
            .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
            .collect();
        let reader = make_clip_reader_at(
            &samples,
            22_046,
            Beat::new(4.0),
            Some(BeatDuration::new(4.0)),
        );
        let mut direct = Direct::new(reader, SampleRate::new(48_000.0), frames);
        for _ in 0..16 {
            direct.block_in(&transport.env(frames, 48_000.0));
        }
        direct
    }

    #[test]
    fn clip_reader_process_steady_state_is_allocation_free() {
        let transport = MockTransport::rolling(Beat::new(5.0), Bpm::new(120.0)); // inside window
        let mut direct = warm_clip_reader(&transport, 64);
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..2_000 {
                direct.block_in(&transport.env(64, 48_000.0));
            }
        });
    }

    /// The jump edge (the playhead jumping within the voice window: the reader
    /// copies the old continuation into its scratch and starts a crossfade)
    /// must be allocation-free. This exercises it *inside* the guarded block
    /// by moving the transport beat each iteration, so a regression that
    /// allocates on the jump path is caught.
    #[test]
    fn clip_reader_jump_edge_is_allocation_free() {
        let transport = MockTransport::rolling(Beat::new(5.0), Bpm::new(120.0)); // inside [4, 8)
        let mut direct = warm_clip_reader(&transport, 64);
        assert_no_alloc::assert_no_alloc(|| {
            for i in 0..2_000 {
                // Jump the playhead back and forth inside the window so the
                // reader keeps taking a scratch copy and starting a fade.
                let beat = if i % 2 == 0 { 5.0 } else { 6.5 };
                transport.set_beat(Beat::new(beat));
                direct.block_in(&transport.env(64, 48_000.0));
            }
        });
    }

    /// One-frame blocks, the shortest a node is handed: the per-block work
    /// (the clock's runs, the ring's claim) allocates nothing either.
    #[test]
    fn clip_reader_single_frame_steady_state_is_allocation_free() {
        let transport = MockTransport::rolling(Beat::new(5.0), Bpm::new(120.0));
        let mut direct = warm_clip_reader(&transport, 1);
        for _ in 0..256 {
            direct.block_in(&transport.env(1, 48_000.0));
        }
        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..100_000 {
                direct.block_in(&transport.env(1, 48_000.0));
            }
        });
    }

    // --- Existing tests ---

    // --- New: play/stop state ---

    #[test]
    fn play_stop_controls_output() {
        let samples: Vec<_> = (0..100).map(|i| (i as f32, -(i as f32))).collect();
        let (mut unit, _state) = make_unit(&samples);

        assert!(unit.is_playing());

        // Play while playing — should produce non-zero after history primes.
        free(&mut unit, 5);

        unit.stop();
        assert!(!unit.is_playing());
        let out = free(&mut unit, 1);
        assert_eq!(out[0][0], 0.0, "stopped unit must output silence");
        assert_eq!(out[1][0], 0.0);

        unit.play();
        assert!(unit.is_playing());
        let out = free(&mut unit, 1);
        assert_ne!(out[0][0], 0.0, "resumed unit should produce audio");
    }

    // --- New: one-frame blocks read interpolated output from the ring ---

    #[test]
    fn single_frames_read_from_ring_buffer_and_interpolate() {
        // Feed a ramp 0,1,2,...,19 into the ring buffer. After enough
        // frames to prime the 4-sample history, output should be
        // non-zero and monotonically increasing (speed=1, src_ratio=1).
        let samples: Vec<_> = (0..20).map(|i| (i as f32, i as f32)).collect();
        let (mut unit, _state) = make_unit(&samples);

        let mut prev = f32::NEG_INFINITY;
        for i in 0..16 {
            let out = free(&mut unit, 1)[0][0];
            if i >= 4 {
                assert!(
                    out >= prev,
                    "ramp should be monotonic at frame {i}: prev={prev}, got={out}"
                );
            }
            prev = out;
        }
        assert!(prev > 0.0, "should have produced non-zero audio");
    }

    // --- New: process block reads from ring buffer ---

    #[test]
    fn process_block_produces_output() {
        let samples: Vec<_> = (0..256)
            .map(|i| (i as f32 * 0.01, -(i as f32) * 0.01))
            .collect();
        let (mut unit, _state) = make_unit(&samples);

        let output = free(&mut unit, 64);

        // After 64 frames at speed=1, output should contain interpolated
        // samples from the ramp. Check last few are non-zero.
        let last = output[0][63];
        assert!(last > 0.0, "process() should produce audio, got {last}");
    }

    #[test]
    fn process_block_silence_when_stopped() {
        let samples: Vec<_> = (0..256).map(|i| (i as f32, 0.0)).collect();
        let (mut unit, _state) = make_unit(&samples);
        unit.stop();

        let output = free(&mut unit, 16);

        for ch in &output[..2] {
            assert_eq!(ch.len(), 16);
            assert!(ch.iter().all(|&s| s == 0.0));
        }
    }

    // --- New: gain application ---

    #[test]
    fn gain_scales_the_output() {
        let samples: Vec<_> = (0..20).map(|_| (1.0f32, -1.0f32)).collect();

        let reader1 = make_reader_with_samples(&samples);
        let reader2 = make_reader_with_samples(&samples);
        let state1 = Arc::new(RtState::new());
        let state2 = Arc::new(RtState::new());

        let mut full = DiskSource::new(reader1, state1);
        let mut half = DiskSource::new(reader2, state2);
        half.set_gain(Amplitude::new(0.5));

        // Prime history then compare
        let out_full = free(&mut full, 6)[0][5];
        let out_half = free(&mut half, 6)[0][5];

        assert!(out_full.abs() > 1e-6, "the full-gain source sounds");
        let ratio = out_half / out_full;
        assert!(
            (ratio - 0.5).abs() < 0.05,
            "gain=0.5 should halve output: full={out_full}, half={out_half}, ratio={ratio}"
        );
    }

    // --- New: reset clears interpolation state ---

    #[test]
    fn reset_clears_state() {
        let samples: Vec<_> = (0..100).map(|i| (i as f32, 0.0)).collect();
        let (mut unit, _state) = make_unit(&samples);

        free(&mut unit, 10);

        unit.reset();
        assert!(!unit.is_playing());
        assert!(
            unit.read.forgot(),
            "the reader still holds its last position"
        );
    }

    /// **A free-running source follows a relayed seek**, from its next block
    /// (`Command::Seek` reaches it as `Ring::request_seek`), and a source taken
    /// after a seek starts there, not at the stream's origin.
    ///
    /// Mutation (run): the seek epoch not compared (`applied_seek` never set)
    /// → the source re-seeks every block and repeats frame 100 → fails.
    /// Mutation (run): `applied_seek` starting at the current epoch → the late
    /// source starts at 0 → fails.
    #[test]
    fn a_free_running_source_follows_a_relayed_seek() {
        let samples: Vec<_> = (0..512).map(|i| (i as f32, 0.0)).collect();
        let (mut unit, _state) = make_unit(&samples);
        let ring = Arc::clone(unit.read.ring());
        free(&mut unit, 8);
        ring.request_seek(100);
        // No fade on this bare ring (its fade length is 0): the next block
        // reads the target at once, and the one after reads on from it.
        for want in [100.0, 108.0] {
            assert_eq!(free(&mut unit, 8)[0][0], want);
        }
        // A source taken after a seek: on a stream of its own, since a ring
        // serves one live reader (this test took a second off the same ring
        // before that rule).
        let seeked = make_reader_with_samples(&samples);
        seeked.request_seek(100);
        let mut late = DiskSource::new(seeked, Arc::new(RtState::new()));
        assert_eq!(free(&mut late, 1)[0][0], 100.0, "starts at the seek");
    }

    /// Build a `channels`-wide ring pre-filled with `frames` interleaved frames.
    fn make_wide_reader(frames: &[f32], channels: usize) -> SharedReader {
        let (mut writer, reader) = RegionBuffer::with_capacity(
            RegionId(1),
            PathBuf::new(),
            frames.len() / channels + 64,
            channels,
        );
        writer.push_interleaved(frames);
        reader
    }

    /// Outside its transport window a voice must silence EVERY channel.
    ///
    /// An `if output.len() >= 2 { .. }` guard silences nothing at all at width
    /// 1, so a mono streaming voice outside its window leaks whatever the
    /// caller's buffer already held; at width 6 the same site writes only
    /// channels 0 and 1, leaving 2..6 holding the previous block. Both are
    /// invisible at width 2, which is what every other streaming fixture uses.
    #[test]
    fn outside_the_window_silences_every_channel() {
        for width in [1usize, 6] {
            let frames: Vec<f32> = (0..64 * width).map(|i| (i + 1) as f32).collect();
            let ring = make_wide_reader(&frames, width);
            let state = Arc::new(RtState::new());
            let inner = DiskSource::new(ring, state.clone());
            assert_eq!(
                inner.channels(),
                ChannelLayout::from(width),
                "ring width must reach the unit"
            );

            // Transport parked BEFORE the voice's start beat, so the placement
            // gate reports "outside".
            let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
            let voice = DiskVoice::new(
                inner,
                state,
                DiskVoiceConfig {
                    window: VoiceWindow {
                        start: Beat::new(64.0),
                        duration: None,
                    },
                    file_sample_rate: SampleRate(44100.0),
                },
            );

            // Pre-dirty the node's outputs so a missing write shows, in a
            // one-frame block and an eight-frame one.
            let mut direct = Direct::new(voice, SampleRate(SR), 8);
            for frames in [1usize, 8] {
                for ch in direct.outputs_mut() {
                    ch.fill(9.0);
                }
                let mut env = transport.env(8, SR);
                env.block_len = Samples(frames);
                if frames == 8 {
                    direct.block_in(&env);
                } else {
                    // `Direct` runs its own block length; a one-frame block
                    // is a node of its own.
                    let mut one = Direct::new(direct.node.clone(), SampleRate(SR), 1);
                    for ch in one.outputs_mut() {
                        ch.fill(9.0);
                    }
                    one.block_in(&env);
                    for c in 0..width {
                        assert_eq!(
                            one.output(c)[0],
                            0.0,
                            "width {width}, one frame: channel {c} not silenced"
                        );
                    }
                    continue;
                }
                for c in 0..width {
                    for i in 0..frames {
                        assert_eq!(
                            direct.output(c)[i],
                            0.0,
                            "width {width}: channel {c} sample {i} not silenced"
                        );
                    }
                }
            }
        }
    }

    /// The ring's stride must index its slots exactly as `channels.count()`
    /// would — at a width where getting it wrong is audible.
    ///
    /// Width 6 and a constant-per-channel ring make a stride error *visible*:
    /// the slots are frame-major, so a wrong stride reads tap `t` of channel
    /// `c` from a different channel's slot and every output carries a
    /// neighbour's value. At width 2 a stride bug and a correct stride coincide
    /// for several access patterns, which is exactly why this is not a stereo
    /// test.
    #[test]
    fn cached_stride_reads_every_channel_at_width_six() {
        let width = 6usize;
        // Every frame is [1, 2, 3, 4, 5, 6]: constant per channel, distinct
        // across channels. Constant in time so cubic interpolation over any four
        // taps of one channel returns that channel's own value exactly — the
        // output then names which channel each slot was read from.
        let frames: Vec<f32> = (0..128)
            .flat_map(|_| (1..=width).map(|c| c as f32))
            .collect();
        let ring = make_wide_reader(&frames, width);
        let state = Arc::new(RtState::new());
        let mut unit = DiskSource::new(ring, state.clone());

        assert_eq!(
            unit.channels(),
            ChannelLayout::from(6u16),
            "the ring's declared width must reach the unit as a layout"
        );
        assert_eq!(
            unit.shape().audio_out.count() as usize,
            width,
            "the cached stride must agree with the declared layout"
        );

        // The block reads frames 0..64, the ring's own.
        let buf = free(&mut unit, 64);
        // The last quarter of the block is well past priming.
        for i in 48..64 {
            for (c, ch) in buf.iter().enumerate().take(width) {
                let got = ch[i];
                let want = (c + 1) as f32;
                assert!(
                    (got - want).abs() < 1e-4,
                    "sample {i} channel {c}: read {got}, want {want} — \
                     a wrong stride reads a neighbouring channel's tap"
                );
            }
        }

        // One-frame blocks read through the same path, so they must land on
        // the same frame.
        let mut frame = free(&mut unit, 1);
        for _ in 0..7 {
            frame = free(&mut unit, 1);
        }
        for (c, ch) in frame.iter().enumerate() {
            let want = (c + 1) as f32;
            assert!(
                (ch[0] - want).abs() < 1e-4,
                "one-frame block, channel {c}: read {}, want {want}",
                ch[0]
            );
        }
    }

    /// **A gain change must reach a voice that is already rendering**, written
    /// through a clone of it — and the clone renders nothing.
    ///
    /// The clone half of the live-value rule, at the disk tier: a gain stored
    /// **by value** in `DiskSource` would be written on one copy and never
    /// reach the one that renders. The gain lives in the stream's control
    /// cell (`RtState`), which every clone shares (a fork detaches it,
    /// `fork_copy`).
    ///
    /// The graph renders the node it was given, and the only copy it takes
    /// (a fork, `fork_copy`) never renders the live stream, so the reader is
    /// the original's alone. What the graph does, and this pins: the original renders at the
    /// gain a copy wrote (a copy holding the cell is how a host's handle,
    /// `DiskVoiceControls`, reaches it), and a clone, holding no reader,
    /// renders silence — it cannot claim the live ring.
    ///
    /// Mutation (run): `LiveRead::clone` keeping the reader (a shared
    /// `Arc<Mutex<PosReader>>` again) → the clone renders the stream → fails.
    /// Mutation (run): `DiskSource::set_gain` writing a by-value field instead
    /// of the shared cell → the original renders at unity → fails.
    #[test]
    fn a_gain_change_reaches_a_cloned_voice() {
        let (mut unit, _state) = make_unit(&[(1.0, 1.0); 256]);
        unit.prepare(&Prepare::new(SampleRate(48_000.0), Samples(64)));
        unit.play();

        // The clone stands in for a copy the host writes through; the
        // original is what the graph renders.
        let mut copy = unit.clone();
        copy.set_gain(Amplitude::new(0.25));

        let peak = |u: &mut DiskSource| {
            let mut peak = 0.0f32;
            for _ in 0..4 {
                for s in &free(u, 8)[0] {
                    peak = peak.max(s.abs());
                }
            }
            peak
        };
        let rendered = peak(&mut unit);
        assert!(
            (rendered - 0.25).abs() < 1e-4,
            "a gain written on one copy of the voice must be seen by the copy \
             that renders; expected ~0.25, got {rendered}. A value near 1.0 \
             means `gain` is stored by value and the write went nowhere."
        );
        let cloned = peak(&mut copy);
        assert_eq!(
            cloned, 0.0,
            "a clone must hold no reader of the live ring, so it renders \
             silence; it rendered {cloned}"
        );
    }
}
