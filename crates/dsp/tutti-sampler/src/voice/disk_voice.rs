//! Disk streaming sample playback, fed by the butler thread.
//!
//! Two types, one tier: [`DiskSource`] is the `AudioUnit` that reads the
//! butler's ring free-running (its own position, stepping by the read rate),
//! and [`DiskVoice`] is a timeline clip over the same stream: it seats on the
//! transport clock exactly as the memory tier's placed read does (`Seat`, the
//! same gate and step) and reads the ring at that position. Both read through
//! `LiveRead`: the ring indexed by position, the memory tier's tap layout, the
//! one kernel — so a clip plays what the same clip in memory plays at the same
//! clock frame, and a jump is a crossfade from where it was, not a flush.
//!
//! Every count crossing the ring is in **frames**. Nothing on the
//! `tick`/`process` path allocates, locks, or blocks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::MAX_SAMPLER_CHANNELS;
use tutti_core::SignalFrame;
use tutti_core::{
    Amplitude, AudioUnit, Beat, BeatDuration, BufferMut, BufferRef, ChannelLayout, PlaybackRate,
    ReadRate, SampleRate, SrcRatio, Timeline,
};

use super::interp::Seat;
use super::live_read::LiveRead;
use super::memory_source::VoiceWindow;
use super::offline_read::OfflineRead;
use super::types::Direction;
use crate::butler::control::StreamOrigin;
use crate::butler::{RtState, SharedReader};
use tutti_core::{FaultLatch, RenderFault};

/// Frames a block's positions are computed for at a time: `process` renders a
/// longer block in pieces this long, so the positions live in a fixed array.
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
/// The audio thread holds the ring through a `SharedReader` (an `Arc`):
/// every access is an atomic, so no lock sits on `tick`/`process`.
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
    /// Width comes from the ring (narrowed to [`MAX_SAMPLER_CHANNELS`], the
    /// widest frame the read path stacks): the file's own layout.
    pub(crate) fn new(consumer: SharedReader, shared_state: Arc<RtState>) -> Self {
        let stride = (consumer.channels().count() as usize).clamp(1, MAX_SAMPLER_CHANNELS);
        let channels = ChannelLayout::from(stride);
        Self {
            read: Box::new(LiveRead::new(consumer, stride)),
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

    /// Start reading. One relaxed atomic store, safe from the audio thread.
    pub fn play(&self) {
        self.playing.store(true, Ordering::Relaxed);
    }

    /// Emit silence and stop reading. The butler keeps its window where the
    /// reader last played.
    pub fn stop(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    /// Whether frames are being read. Both `tick` and `process` check this
    /// before touching the ring, which is what makes clearing it a complete
    /// severing in [`isolate`](AudioUnit::isolate).
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Publish a new output gain, to the shared `RtState` (see `tutti_nodes`'
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

    /// Forget the position and any fade: the next block starts afresh.
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
            self.read
                .render(&positions[..n], rate, gain, &state, |i, f| {
                    emit(done + i, f)
                });
            position += rate * n as f64;
            done += n;
        }
        self.position = Some(position);
    }
}

impl AudioUnit for DiskSource {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        // Boundary: `AudioUnit::outputs` is a fixed fundsp trait signature.
        self.stride
    }

    fn reset(&mut self) {
        self.playing.store(false, Ordering::Relaxed);
        self.reset_interpolation();
    }

    /// Stop this clone from touching the live stream.
    ///
    /// `Clone` shares the ring and the control state by `Arc` — correct for a
    /// clone that stays in the live graph, wrong for one taken to render
    /// offline: its reads would publish a position the butler follows, moving
    /// the live voice's window. Both `tick` and `process` return silence before
    /// touching the ring when `playing` is false, and `shared_state` is
    /// dropped, so clearing both is a complete severing.
    ///
    /// The honest severed state of this bare unit is *silent*: there is no
    /// second ring to hand this clone. A [`DiskVoice`], which knows where on
    /// the timeline it plays and which stream it came from, does not stop
    /// there: its copy reads the file itself (see its `isolate`).
    fn isolate(&mut self) {
        self.playing.store(false, Ordering::Relaxed);
        self.shared_state = None;
        self.reset_interpolation();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        let n = self.stride.min(output.len());
        if n == 0 {
            return;
        }
        if !self.playing.load(Ordering::Relaxed) {
            output[..n].fill(0.0);
            return;
        }
        self.render(1, |_, f| output[..n].copy_from_slice(&f[..n]));
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        let n = self.stride.min(output.channels());
        if !self.playing.load(Ordering::Relaxed) {
            for c in 0..n {
                for i in 0..size {
                    output.set_f32(c, i, 0.0);
                }
            }
            return;
        }
        self.render(size, |i, f| {
            for (c, &s) in f[..n].iter().enumerate() {
                output.set_f32(c, i, s);
            }
        });
    }

    audio_unit_boilerplate!(id = crate::node_id::STREAMING_SAMPLER_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        SignalFrame::new(self.outputs())
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

// ---------------------------------------------------------------------------
// DiskVoice — a timeline clip over a stream.
//
// Per frame: the seat (`Seat::next`, the memory tier's) gives the position the
// clock puts the playhead at inside the window, `None` outside it or on a
// stopped clock; the ring is read there. The butler fills ahead of where the
// voice reads; a jump is the reader's own crossfade (`LiveRead`).
// ---------------------------------------------------------------------------

/// Wiring for [`DiskVoice::new`]: the placement gate plus the file
/// sample rate. Splits the clock from the [`VoiceWindow`]
/// cluster so the streaming path and `MemorySource` speak the same value type;
/// `shared_state` stays a separate wiring arg (it must be the same `RtState` the
/// `inner` unit holds).
// Hand-rolled `Debug` for the same reason as `MemorySourceConfig`: an
// `Arc<dyn Timeline>` is not `Debug`.
#[derive(Clone)]
pub struct DiskVoiceConfig {
    /// Transport clock — the gate reads its beat position.
    pub timeline: Arc<dyn Timeline>,
    /// Span of timeline this voice occupies. Separate from the clock, mirroring
    /// `MemorySource` — see [`VoiceWindow`].
    pub window: VoiceWindow,
    /// File sample rate — converts the transport's second-offset into a file
    /// position, matching `MemorySource`'s use of `wave.sample_rate()`.
    ///
    /// The rate the butler recorded from the file's header when it opened the
    /// stream (`Status::take_disk_voice`), not one recovered from the session
    /// rate: that one moves on a device restart, and the file's does not.
    pub file_sample_rate: SampleRate,
}

impl std::fmt::Debug for DiskVoiceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskVoiceConfig")
            .field("window", &self.window)
            .field("file_sample_rate", &self.file_sample_rate)
            .finish_non_exhaustive()
    }
}

/// A [`DiskSource`]'s stream, placed on the timeline.
///
/// A timeline clip must sound only while the playhead is inside its
/// `[start, start + duration)` window, and there play the file frame the
/// clock puts the playhead on. This does exactly what the memory tier's placed
/// read does — the same gate ([`window_position`](super::interp::window_position))
/// seated and stepped the same way (`Seat`) — and reads the ring at the
/// position that gives, so live, forked and in-memory voices of one clip play
/// the same samples at the same clock frame.
///
/// # A fork reads the file itself
///
/// A copy taken for an offline render (a graph fork for an export) cannot
/// read the ring: its reads would move the live voice's window. So
/// [`isolate`](AudioUnit::isolate) cuts this copy off, and
/// [`rebind_offline`](AudioUnit::rebind_offline) hands it the stream's file,
/// as the butler records it (path, rate, loop), to read on demand on the
/// render's thread. It plays the same window of the file on the render's
/// timeline, read as the memory tier reads it; see `offline_read`. That path
/// blocks on file I/O and is never taken live.
pub struct DiskVoice {
    inner: DiskSource,
    /// The playback controls: the butler's cell, shared with `inner`, while
    /// live; a private snapshot of it once isolated.
    shared_state: Arc<RtState>,

    /// Transport clock — the gate reads its beat position.
    timeline: Arc<dyn Timeline>,
    /// Span of timeline this voice occupies.
    window: VoiceWindow,

    /// File sample rate — converts the transport's second-offset into a file
    /// position, matching `MemorySource`'s use of `wave.sample_rate()`.
    file_sample_rate: SampleRate,

    /// Where the clock last seated the live read. `None` outside the window.
    seat: Option<Seat>,

    /// The butler stream this voice consumes, as a read-only handle onto the
    /// butler's record of it: what a fork reads its file from. `None` for a
    /// voice not built by [`Status::take_disk_voice`](crate::Status::take_disk_voice)
    /// (a test's bare ring), whose fork then plays silence.
    origin: Option<StreamOrigin>,

    /// `Some` once this copy is severed from the live stream
    /// ([`isolate`](AudioUnit::isolate)): it then never touches the ring or
    /// the butler again, and plays what this holds instead. Boxed: a live
    /// voice (every voice in a pool) should not carry the pages' room, and
    /// it is built on the control thread, where a fork is taken.
    offline: Option<Box<Offline>>,

    /// The rate `set_sample_rate` last gave this unit, `None` until one did:
    /// the rate the step converts to. A severed copy never told a rate renders
    /// nothing and says so rather than play off pitch; a live voice never told
    /// one steps by the butler's conversion for the session rate.
    sample_rate: Option<SampleRate>,
}

/// A severed disk voice's own playback: the file it reads, where in the file
/// the clock last seated it, and where its failures go.
#[derive(Clone, Debug, Default)]
struct Offline {
    /// The file, from the butler's record at
    /// [`rebind_offline`](AudioUnit::rebind_offline). `None` before a rebind,
    /// or when the stream was gone by then: silence.
    read: Option<OfflineRead>,
    /// Where the clock last seated the read. `None` outside the window.
    seat: Option<Seat>,
    /// The first failure since this copy was severed (its stream gone, its
    /// file unreadable, no render rate), handed to the fork by
    /// [`render_fault`](AudioUnit::render_fault). Fresh at every `isolate`,
    /// so a copy never reports another's.
    fault: Arc<FaultLatch>,
}

/// Why a severed disk voice renders silence where its file should be, other
/// than the file itself (`OfflineReadError`).
#[derive(Debug)]
enum OfflineFault {
    /// Its stream ended (stopped, or its channel restarted on another file)
    /// before the copy was rebound.
    StreamGone,
    /// It was asked to render before `set_sample_rate` gave it a rate.
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

// Hand-rolled: wraps a non-`Debug` `Arc<dyn Timeline>`.
impl std::fmt::Debug for DiskVoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiskVoice")
            .field("inner", &self.inner)
            .field("window", &self.window)
            .field("file_sample_rate", &self.file_sample_rate)
            .field("seat", &self.seat)
            .field("has_origin", &self.origin.is_some())
            .field("offline", &self.offline)
            .finish_non_exhaustive()
    }
}

impl Clone for DiskVoice {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            shared_state: Arc::clone(&self.shared_state),
            timeline: self.timeline.clone(),
            window: self.window,
            file_sample_rate: self.file_sample_rate,
            seat: self.seat,
            origin: self.origin.clone(),
            offline: self.offline.clone(),
            sample_rate: self.sample_rate,
        }
    }
}

impl DiskVoice {
    /// Place a `DiskSource`'s stream on the timeline.
    ///
    /// Construction (butler stream registration, ring allocation) happens on
    /// the ECS/butler side; this only binds the already-built unit to a
    /// timeline window. `shared_state` must be the same `RtState` the `inner`
    /// unit holds.
    pub fn new(inner: DiskSource, shared_state: Arc<RtState>, config: DiskVoiceConfig) -> Self {
        Self {
            inner,
            shared_state,
            timeline: config.timeline,
            window: config.window,
            file_sample_rate: config.file_sample_rate,
            seat: None,
            origin: None,
            offline: None,
            sample_rate: None,
        }
    }

    /// Record which butler stream this voice consumes, so a fork of it can
    /// read the same file (see "A fork reads the file itself").
    pub(crate) fn with_origin(mut self, origin: StreamOrigin) -> Self {
        self.origin = Some(origin);
        self
    }

    /// The transport clock this voice's gate reads.
    pub fn timeline(&self) -> Arc<dyn Timeline> {
        Arc::clone(&self.timeline)
    }

    /// Tell the stream how fast a wrapping time-stretcher wants its source.
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

    /// Move the voice's window to `[start_beat, start_beat + duration)`, or to
    /// the whole source when `duration` is `None`. The next frame seats afresh.
    pub fn set_placement(&mut self, start_beat: Beat, duration: Option<BeatDuration>) {
        self.window = VoiceWindow {
            start: start_beat,
            duration,
        };
        self.seat = None;
        // And a severed copy re-seats from the clock (a fork applies the
        // placement its node last queued; `VoiceNode::isolate`).
        if let Some(offline) = self.offline.as_mut() {
            offline.seat = None;
        }
    }

    /// Publish a new output gain. `&self`: the write lands in the shared
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

    /// Set the playback speed magnitude, in the shared `RtState` (what the
    /// butler's `SetVarispeed` also sets). The next block reads at the
    /// position the new speed gives, crossfaded.
    pub fn set_speed(&mut self, speed: PlaybackRate) {
        self.shared_state.set_speed(speed);
    }

    /// Set the playback direction, in the shared `RtState`. The butler turns
    /// the ring's mapping on its next cycle (reverse ignores the loop and
    /// mirrors the file, as the memory tier's reverse does).
    pub fn set_direction(&mut self, direction: Direction) {
        self.shared_state.set_direction(direction);
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

    /// The seat for the next frame: the last one stepped while the clock reads
    /// its beat, else a fresh one where the gate puts the playhead; `None`
    /// outside the window or on a stopped clock.
    #[inline]
    fn next_seat(&self, last: Option<Seat>, rate: ReadRate) -> Option<Seat> {
        Seat::next(last, self.timeline.as_ref(), rate, || {
            super::interp::window_position(
                self.timeline.as_ref(),
                self.window.start,
                self.window.duration,
                self.file_sample_rate,
                self.window_rate(),
            )
        })
    }

    /// Render `size` live frames, frame `i` handed to `emit`: the seat's
    /// positions, less the channel's preroll, read from the ring.
    fn live_render(&mut self, size: usize, mut emit: impl FnMut(usize, &[f32])) {
        let rate = self.step_rate();
        let preroll = self.inner.read.ring().preroll() as f64;
        let gain = self.shared_state.gain().get();
        let state = Arc::clone(&self.shared_state);
        let mut positions = [None; BLOCK_FRAMES];
        let mut done = 0;
        while done < size {
            let n = (size - done).min(BLOCK_FRAMES);
            let mut seat = self.seat;
            for p in positions[..n].iter_mut() {
                seat = self.next_seat(seat, rate);
                *p = seat.map(|s| (s.position().get() - preroll).max(0.0));
            }
            self.seat = seat;
            self.inner
                .read
                .render(&positions[..n], rate.get(), gain, &state, |i, f| {
                    emit(done + i, f)
                });
            done += n;
        }
    }

    /// One output frame of a severed copy, into `out` (every element).
    ///
    /// The gate and the seat are the live ones; where the live voice reads the
    /// ring, this reads the file at the seated position. Once the playhead is
    /// past the window's end, the file is closed: a render holding many voices
    /// keeps a file open per voice sounding.
    fn offline_frame(&mut self, out: &mut [f32]) {
        let last = self.offline.as_ref().and_then(|offline| offline.seat);
        // A copy never told a rate latches below, inside its window.
        let rate = self.step_rate();
        let Some(seat) = self.next_seat(last, rate) else {
            let beat = self.timeline.beat();
            let past = self.window.duration.is_some_and(|duration| {
                self.timeline.is_rolling() && beat >= self.window.start + duration
            });
            if let Some(offline) = self.offline.as_mut() {
                offline.seat = None;
                if past {
                    if let Some(read) = offline.read.as_mut() {
                        read.close();
                    }
                }
            }
            out.fill(0.0);
            return;
        };
        let direction = self.shared_state.direction();
        let gain = self.shared_state.gain().get();
        let told_rate = self.sample_rate.is_some();
        let Some(offline) = self.offline.as_mut() else {
            out.fill(0.0);
            return;
        };
        if !told_rate {
            offline.fault.latch(OfflineFault::NoRenderRate);
            out.fill(0.0);
            return;
        }
        let pos = seat.position();
        offline.seat = Some(seat);
        match offline.read.as_mut() {
            Some(read) => read.read_into(pos, direction, out),
            None => out.fill(0.0),
        }
        for s in out.iter_mut() {
            *s *= gain;
        }
    }
}

impl AudioUnit for DiskVoice {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        // Delegate rather than store a second copy: two widths could disagree.
        self.inner.outputs()
    }

    fn reset(&mut self) {
        self.inner.reset_interpolation();
        self.seat = None;
        if let Some(offline) = self.offline.as_mut() {
            offline.seat = None;
        }
    }

    /// Sever this copy from the live stream, whole: it will never read the
    /// ring or publish a position to the butler again.
    ///
    /// `inner` is isolated (it holds the ring), and this voice's own handle
    /// on the stream's control cell is replaced by a private cell holding the
    /// controls' current values (`RtState::detached`), which is also what
    /// makes the controls a snapshot, as every forked unit's are. From here on
    /// `tick`/`process` take the severed path (`offline_frame`), which reads
    /// neither.
    ///
    /// What the copy then plays comes from
    /// [`rebind_offline`](AudioUnit::rebind_offline); until then, silence.
    /// A copy isolated again (a fork of an isolated shadow) keeps the file it
    /// was handed.
    fn isolate(&mut self) {
        self.inner.isolate();
        self.shared_state = Arc::new(self.shared_state.detached());
        self.seat = None;
        let fault = Arc::new(FaultLatch::default());
        let read = self
            .offline
            .take()
            .and_then(|offline| offline.read)
            .map(|read| read.relatched(Arc::clone(&fault)));
        self.offline = Some(Box::new(Offline {
            read,
            seat: None,
            fault,
        }));
    }

    /// The copy's failure latch, once it is severed; `None` live.
    fn render_fault(&self) -> Option<Arc<dyn RenderFault>> {
        self.offline
            .as_ref()
            .map(|offline| Arc::clone(&offline.fault) as Arc<dyn RenderFault>)
    }

    /// Re-point the placement gate's clock at the render's transport, and
    /// hand this copy the stream's file to read.
    ///
    /// The file is the one the butler's record of the stream names **now**
    /// (see `StreamOrigin`), with the loop set on it now: the moment a graph
    /// fork is taken, since a fork calls this right after `isolate`. A voice
    /// rebound without having been isolated is isolated first: one on the
    /// render's clock must never move the live stream. The handle on the
    /// stream's record is dropped once read — the render never needs it
    /// again. A stream that has ended by now is a latched failure
    /// ([`render_fault`](AudioUnit::render_fault)): the export fails naming
    /// the voice rather than write its silence.
    ///
    /// Takes the stream record's lock, so control thread only, as every
    /// rebind is.
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        let Some(transport) = ctx.downcast_ref::<tutti_core::transport::OfflineTransport>() else {
            return;
        };
        if self.offline.is_none() {
            self.isolate();
        }
        self.timeline = transport.clone();
        self.seat = None;
        let file = self.origin.take().map(|origin| origin.describe());
        let offline = self.offline.get_or_insert_with(Box::default);
        offline.seat = None;
        match file {
            Some(Some(file)) => {
                offline.read = Some(OfflineRead::new(file, Arc::clone(&offline.fault)));
            }
            Some(None) => {
                offline.fault.latch(OfflineFault::StreamGone);
                offline.read = None;
            }
            // Nothing to read from (a voice over a bare ring), or rebound
            // before: keep what it has.
            None => {}
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.inner.set_sample_rate(sample_rate);
        self.sample_rate = Some(sample_rate);
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        let n = self.outputs().min(output.len());
        if self.offline.is_some() {
            self.offline_frame(&mut output[..n]);
            return;
        }
        self.live_render(1, |_, f| output[..n].copy_from_slice(&f[..n]));
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        let n = self
            .outputs()
            .min(output.channels())
            .min(MAX_SAMPLER_CHANNELS);
        if self.offline.is_some() {
            // A frame at a time, as `tick` reads it: one path, so the two
            // entry points cannot come apart.
            let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
            for i in 0..size {
                self.offline_frame(&mut frame[..n]);
                for (c, &s) in frame[..n].iter().enumerate() {
                    output.set_f32(c, i, s);
                }
            }
            return;
        }
        self.live_render(size, |i, f| {
            for (c, &s) in f[..n].iter().enumerate() {
                output.set_f32(c, i, s);
            }
        });
    }

    audio_unit_boilerplate!(id = crate::node_id::STREAMING_SAMPLER_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        SignalFrame::new(self.outputs())
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod live_loop;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::butler::{RegionBuffer, RegionId};
    use std::path::PathBuf;
    use tutti_core::{BufferVec, SamplePosition, Timeline};

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
        reader.reset(start);
        reader.set_play(start);
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
    /// The FIFO reader this replaced popped four frames of interpolation
    /// head-room per block and dropped them, so it ran 68/64 fast — every
    /// streamed file a quarter-tone sharp with its level and waveform intact.
    /// Reading by position has no fetch step to get wrong, but the step is
    /// still the one quantity that decides pitch, so it stays pinned.
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

        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        for block in 0..BLOCKS {
            unit.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
            for i in 0..BLOCK {
                let frame = block * BLOCK + i;
                assert_eq!(
                    output.buffer_ref().at_f32(0, i),
                    samples[frame].0,
                    "output frame {frame}"
                );
            }
        }
        assert_eq!(unit.read.read_to, (BLOCK * BLOCKS - 1) as f64);
    }

    // --- DiskVoice: placement gate ---

    use crate::test_transport::MockTransport;
    use tutti_core::Bpm;

    /// A voice at 44.1 kHz over a ring holding `samples` from straight
    /// position 0.
    fn make_clip_reader(
        samples: &[(f32, f32)],
        transport: Arc<dyn Timeline>,
        start_beat: Beat,
        duration: Option<BeatDuration>,
    ) -> DiskVoice {
        make_clip_reader_at(samples, 0, transport, start_beat, duration)
    }

    /// [`make_clip_reader`] over a ring holding `samples` from `at` on.
    fn make_clip_reader_at(
        samples: &[(f32, f32)],
        at: u64,
        transport: Arc<dyn Timeline>,
        start_beat: Beat,
        duration: Option<BeatDuration>,
    ) -> DiskVoice {
        let state = Arc::new(RtState::new());
        let inner = DiskSource::new(make_reader_at(samples, at), Arc::clone(&state));
        DiskVoice::new(
            inner,
            state,
            DiskVoiceConfig {
                timeline: transport,
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
                timeline: transport.clone(),
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
        reader.set_sample_rate(SampleRate(44_100.0));
        reader.tick(&[], &mut [0.0f32; 2]);
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
    /// rolling. Before the clock moves the read goes on from where it stands
    /// at the new rate (the seat re-anchors, as the memory tier's does); once
    /// it moves, the voice reads where the gate puts the playhead at the new
    /// speed and tells the butler so (`Ring::play`), which fills there.
    ///
    /// Mutation (run): the gate's rate ignoring varispeed → the voice reads
    /// on near 441 000 → fails.
    #[test]
    fn a_varispeed_change_moves_the_read_without_any_beat_discontinuity() {
        let samples: Vec<_> = (1..4096)
            .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
            .collect();
        let transport = MockTransport::rolling(Beat::new(20.0), Bpm::new(120.0));
        let mut reader = make_clip_reader(&samples, transport.clone(), Beat::new(0.0), None);
        let ring = Arc::clone(reader.inner.read.ring());

        let mut out = [0.0f32; 2];
        reader.tick(&[], &mut out);
        let before = ring.play();
        assert_eq!(before, 441_000, "10 s of a 44.1 kHz file");

        let beat_before = transport.beat();
        reader.set_speed(PlaybackRate::new(2.0));
        reader.tick(&[], &mut out);
        assert_eq!(
            transport.beat(),
            beat_before,
            "the playhead must not have moved; otherwise this proves nothing"
        );
        // Re-anchored where it stood: one step at the new rate on.
        assert_eq!(ring.play(), 441_002, "the seat re-anchors where it stands");
        // One frame on: the gate's position at 2x.
        transport.advance(1, 44_100.0);
        reader.tick(&[], &mut out);
        assert_eq!(ring.play(), 882_002, "at 2x beat 20 is 20 s in");
    }

    /// **A severed copy past its window closes its file**, and one inside it
    /// holds it open: a render of many voices keeps a file open per voice
    /// sounding. Mutation (run): the close removed from `offline_frame`'s
    /// past-the-window branch → still open → fails.
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
        let live: Arc<dyn Timeline> = MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0));
        // Beats [0, 1): 24 000 frames at 120 BPM.
        let voice = streamer
            .status()
            .take_disk_voice(0, live, Beat::new(0.0), Some(BeatDuration::new(1.0)))
            .expect("the link is installed");

        let render = Arc::new(tutti_core::transport::OfflineTimeline::new(
            &tutti_core::transport::OfflineTimelineConfig {
                start_beat: Beat::new(0.0),
                tempo: Bpm::new(120.0),
                sample_rate: SampleRate(48_000.0),
                loop_range: None,
            },
        ));
        let ctx: tutti_core::transport::OfflineTransport = render.clone();
        let mut copy = voice.clone();
        copy.isolate();
        copy.rebind_offline(&ctx);
        copy.reset();
        copy.set_sample_rate(SampleRate(48_000.0));
        let is_open = |copy: &DiskVoice| {
            copy.offline
                .as_ref()
                .and_then(|offline| offline.read.as_ref())
                .is_some_and(OfflineRead::is_open)
        };
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        let mut played = 0;
        while played < 30_000 {
            copy.process(64, &input.buffer_ref(), &mut output.buffer_mut());
            played += 64;
            render.advance(64);
            if played == 1_024 {
                assert!(is_open(&copy), "the file is open while the clip plays");
            }
        }
        assert!(!is_open(&copy), "the file is still open past the window");
    }

    /// **A stopped clock silences a fork mid-clip**, through `process` and
    /// through `tick`: the clock stops where it stands (its beat does not
    /// move), and the read must not run on from its seat as if it still
    /// rolled. A render's own clock always rolls, so the fork is put on a
    /// mock one after its rebind; the seat is the memory tier's too
    /// (`memory_source`'s `a_stopped_clock_silences_a_placed_read`).
    ///
    /// Mutation (run): the `is_rolling` guard removed from `Seat::next` → the
    /// seat runs on through the stop → fails.
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
        let live: Arc<dyn Timeline> = MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0));
        let voice = streamer
            .status()
            .take_disk_voice(0, live, Beat::new(0.0), None)
            .expect("the link is installed");
        let render = Arc::new(tutti_core::transport::OfflineTimeline::new(
            &tutti_core::transport::OfflineTimelineConfig {
                start_beat: Beat::new(0.0),
                tempo: Bpm::new(120.0),
                sample_rate: SampleRate(48_000.0),
                loop_range: None,
            },
        ));
        let ctx: tutti_core::transport::OfflineTransport = render;

        for via_tick in [false, true] {
            let mut copy = voice.clone();
            copy.isolate();
            copy.rebind_offline(&ctx);
            copy.reset();
            copy.set_sample_rate(SampleRate(48_000.0));
            let clock = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
            copy.timeline = clock.clone();
            let block = |copy: &mut DiskVoice| -> Vec<f32> {
                if via_tick {
                    (0..64)
                        .map(|_| {
                            let mut frame = [0.0f32; 2];
                            copy.tick(&[], &mut frame);
                            frame[0]
                        })
                        .collect()
                } else {
                    let input = BufferVec::new(0);
                    let mut output = BufferVec::new(2);
                    copy.process(64, &input.buffer_ref(), &mut output.buffer_mut());
                    (0..64).map(|i| output.buffer_ref().at_f32(0, i)).collect()
                }
            };
            assert!(
                block(&mut copy).iter().all(|&s| s != 0.0),
                "tick {via_tick}: rolling, the clip plays"
            );
            clock.set_rolling(false);
            assert!(
                block(&mut copy).iter().all(|&s| s == 0.0),
                "tick {via_tick}: stopped, the fork plays on"
            );
        }
    }

    /// **A copy severed for an offline render never touches the live stream**:
    /// rendered inside its window on the render's clock, it tells the live
    /// butler no position (so the live window stays where the live voice
    /// reads), and its controls are its own. A copy only *rebound* (no `isolate` first) is severed too.
    /// So a disk voice, and a `VoiceNode` holding one, can be forked
    /// (`forkable`, which a fork trusts). What such a copy plays instead is
    /// `tests/offline_disk_voice.rs`'s.
    ///
    /// Mutation (run): `isolate` keeping the live `shared_state` → the copy's
    /// gain write lands on the live cell → fails. Mutation (run):
    /// `rebind_offline` not isolating a live copy first → that copy reads the
    /// live ring on the render's clock and publishes its position → fails.
    #[test]
    fn a_severed_copy_never_touches_the_live_stream() {
        let samples: Vec<_> = (1..4096).map(|i| (i as f32, i as f32)).collect();
        let live_clock = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let live = make_clip_reader(&samples, live_clock, Beat::new(0.0), None);
        let ring = Arc::clone(live.inner.read.ring());
        let (play_before, window_before) = (ring.play(), ring.window());

        let render: tutti_core::transport::OfflineTransport =
            MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
        let mut isolated = live.clone();
        isolated.isolate();
        isolated.rebind_offline(&render);
        let mut rebound_only = live.clone();
        rebound_only.rebind_offline(&render);

        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        for copy in [&mut isolated, &mut rebound_only] {
            copy.reset();
            for _ in 0..16 {
                copy.process(64, &input.buffer_ref(), &mut output.buffer_mut());
            }
            copy.set_gain(Amplitude::new(0.25));
        }

        assert_eq!(ring.play(), play_before, "a copy moved the live read");
        assert_eq!(ring.window(), window_before, "a copy touched the live ring");
        assert_eq!(
            live.gain(),
            Amplitude::new(1.0),
            "a copy moved the live gain"
        );
        assert!(live.forkable());
        let node = crate::voice::node::VoiceNode::with_channels(
            crate::voice::types::Voice {
                source: crate::voice::types::VoiceSource::Disk(live),
                play: crate::voice::types::Playback::default(),
                channel_index: None,
            },
            2usize,
        );
        assert!(node.forkable());
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
            transport.clone(),
            Beat::new(4.0),
            Some(BeatDuration::new(4.0)),
        );

        // Before the window (beat 0): silence, ring untouched.
        let mut out = [0.0f32; 2];
        for _ in 0..8 {
            reader.tick(&[], &mut out);
        }
        assert_eq!(out, [0.0, 0.0], "before window → silence");

        // Inside the window (beat 5): reads the ring, produces audio.
        transport.set_beat(Beat::new(5.0));
        let mut got_audio = false;
        for _ in 0..8 {
            reader.tick(&[], &mut out);
            if out[0] != 0.0 || out[1] != 0.0 {
                got_audio = true;
            }
        }
        assert!(got_audio, "inside window → audible");

        // Past the window (beat 9): silent again.
        transport.set_beat(Beat::new(9.0));
        reader.tick(&[], &mut out);
        assert_eq!(out, [0.0, 0.0], "after window → silence");
    }

    #[test]
    fn clip_reader_stopped_transport_is_silent() {
        let samples: Vec<_> = (1..32).map(|i| (i as f32, i as f32)).collect();
        let transport = MockTransport::stopped(Beat::new(5.0), Bpm::new(120.0)); // inside window but stopped
        let mut reader = make_clip_reader(
            &samples,
            transport,
            Beat::new(4.0),
            Some(BeatDuration::new(4.0)),
        );

        let mut out = [1.0f32; 2];
        reader.tick(&[], &mut out);
        assert_eq!(out, [0.0, 0.0], "stopped transport → silence");
    }

    // --- DiskVoice: RT no-alloc (steady-state process inside window) ---
    //
    // The integration-test gate `tests/rt_no_alloc.rs` cannot reach the
    // crate-private ring / `RtState` needed to build a streaming reader, so the streaming-variant no-alloc guard lives here, in-crate, with
    // a module-local `AllocDisabler`. The disabler only aborts inside an
    // `assert_no_alloc` region; every other unit test allocates normally.

    #[global_allocator]
    static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

    #[test]
    fn clip_reader_process_steady_state_is_allocation_free() {
        let samples: Vec<_> = (1..2048)
            .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
            .collect();
        let transport = MockTransport::rolling(Beat::new(5.0), Bpm::new(120.0)); // inside window
        let mut reader = make_clip_reader_at(
            &samples,
            22_046,
            transport,
            Beat::new(4.0),
            Some(BeatDuration::new(4.0)),
        );
        reader.set_sample_rate(tutti_core::SampleRate::new(48_000.0));

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        // Warm-up: settles the enter-window seek edge + primes the interpolation
        // history so the guarded loop is on the steady-state path. (The seek edge
        // itself is alloc-free — see `clip_reader_seek_edge_is_allocation_free`.)
        for _ in 0..16 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            reader.process(64, &input, &mut output);
        }

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..2_000 {
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                reader.process(64, &input, &mut output);
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
        let samples: Vec<_> = (1..2048)
            .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
            .collect();
        let transport = MockTransport::rolling(Beat::new(5.0), Bpm::new(120.0)); // inside [4, 8)
        let mut reader = make_clip_reader_at(
            &samples,
            22_046,
            Arc::clone(&transport) as Arc<dyn Timeline>,
            Beat::new(4.0),
            Some(BeatDuration::new(4.0)),
        );
        reader.set_sample_rate(tutti_core::SampleRate::new(48_000.0));

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        // Prime interpolation history / settle the initial enter-window seek.
        for _ in 0..16 {
            let input = input_vec.buffer_ref();
            let mut output = output_vec.buffer_mut();
            reader.process(64, &input, &mut output);
        }

        assert_no_alloc::assert_no_alloc(|| {
            for i in 0..2_000 {
                // Jump the playhead back and forth inside the window so the
                // reader keeps taking a scratch copy and starting a fade.
                let beat = if i % 2 == 0 { 5.0 } else { 6.5 };
                transport.set_beat(Beat::new(beat));
                let input = input_vec.buffer_ref();
                let mut output = output_vec.buffer_mut();
                reader.process(64, &input, &mut output);
            }
        });
    }

    #[test]
    fn clip_reader_tick_steady_state_is_allocation_free() {
        let samples: Vec<_> = (1..2048)
            .map(|i| (i as f32 * 0.001, i as f32 * 0.001))
            .collect();
        let transport = MockTransport::rolling(Beat::new(5.0), Bpm::new(120.0));
        let mut reader = make_clip_reader_at(
            &samples,
            22_046,
            transport,
            Beat::new(4.0),
            Some(BeatDuration::new(4.0)),
        );
        reader.set_sample_rate(tutti_core::SampleRate::new(48_000.0));

        let mut out = [0.0f32; 2];
        for _ in 0..256 {
            reader.tick(&[], &mut out);
        }

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..100_000 {
                reader.tick(&[], &mut out);
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

        // Tick while playing — should produce non-zero after history primes.
        let mut out = [0.0f32; 2];
        for _ in 0..5 {
            unit.tick(&[], &mut out);
        }
        let playing_sample = out[0];

        unit.stop();
        assert!(!unit.is_playing());
        unit.tick(&[], &mut out);
        assert_eq!(out[0], 0.0, "stopped unit must output silence");
        assert_eq!(out[1], 0.0);

        unit.play();
        assert!(unit.is_playing());
        unit.tick(&[], &mut out);
        assert_ne!(out[0], 0.0, "resumed unit should produce audio");
        let _ = playing_sample;
    }

    // --- New: tick produces interpolated output from ring buffer ---

    #[test]
    fn tick_reads_from_ring_buffer_and_interpolates() {
        // Feed a ramp 0,1,2,...,19 into the ring buffer. After enough
        // ticks to prime the 4-sample history, output should be
        // non-zero and monotonically increasing (speed=1, src_ratio=1).
        let samples: Vec<_> = (0..20).map(|i| (i as f32, i as f32)).collect();
        let (mut unit, _state) = make_unit(&samples);

        let mut prev = f32::NEG_INFINITY;
        let mut out = [0.0f32; 2];
        for i in 0..16 {
            unit.tick(&[], &mut out);
            if i >= 4 {
                assert!(
                    out[0] >= prev,
                    "ramp should be monotonic at tick {i}: prev={prev}, got={}",
                    out[0]
                );
            }
            prev = out[0];
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

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        unit.process(64, &input, &mut output);

        // After 64 frames at speed=1, output should contain interpolated
        // samples from the ramp. Check last few are non-zero.
        let last = output.at_f32(0, 63);
        assert!(last > 0.0, "process() should produce audio, got {last}");
    }

    #[test]
    fn process_block_silence_when_stopped() {
        let samples: Vec<_> = (0..256).map(|i| (i as f32, 0.0)).collect();
        let (mut unit, _state) = make_unit(&samples);
        unit.stop();

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        unit.process(16, &input, &mut output);

        for i in 0..16 {
            assert_eq!(output.at_f32(0, i), 0.0);
            assert_eq!(output.at_f32(1, i), 0.0);
        }
    }

    // --- New: gain application ---

    #[test]
    fn gain_scales_tick_output() {
        let samples: Vec<_> = (0..20).map(|_| (1.0f32, -1.0f32)).collect();

        let reader1 = make_reader_with_samples(&samples);
        let reader2 = make_reader_with_samples(&samples);
        let state1 = Arc::new(RtState::new());
        let state2 = Arc::new(RtState::new());

        let mut full = DiskSource::new(reader1, state1);
        let mut half = DiskSource::new(reader2, state2);
        half.set_gain(Amplitude::new(0.5));

        let mut out_full = [0.0f32; 2];
        let mut out_half = [0.0f32; 2];

        // Prime history then compare
        for _ in 0..6 {
            full.tick(&[], &mut out_full);
            half.tick(&[], &mut out_half);
        }

        if out_full[0].abs() > 1e-6 {
            let ratio = out_half[0] / out_full[0];
            assert!(
                (ratio - 0.5).abs() < 0.05,
                "gain=0.5 should halve output: full={}, half={}, ratio={ratio}",
                out_full[0],
                out_half[0]
            );
        }
    }

    // --- New: reset clears interpolation state ---

    #[test]
    fn reset_clears_state() {
        let samples: Vec<_> = (0..100).map(|i| (i as f32, 0.0)).collect();
        let (mut unit, _state) = make_unit(&samples);

        let mut out = [0.0f32; 2];
        for _ in 0..10 {
            unit.tick(&[], &mut out);
        }

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
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        unit.process(8, &input.buffer_ref(), &mut output.buffer_mut());
        ring.request_seek(100);
        // No fade on this bare ring (its fade length is 0): the next block
        // reads the target at once, and the one after reads on from it.
        for want in [100.0, 108.0] {
            unit.process(8, &input.buffer_ref(), &mut output.buffer_mut());
            assert_eq!(output.buffer_ref().at_f32(0, 0), want);
        }
        let mut late = DiskSource::new(Arc::clone(&ring), Arc::new(RtState::new()));
        late.process(1, &input.buffer_ref(), &mut output.buffer_mut());
        assert_eq!(
            output.buffer_ref().at_f32(0, 0),
            100.0,
            "starts at the seek"
        );
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
            let mut voice = DiskVoice::new(
                inner,
                state,
                DiskVoiceConfig {
                    timeline: transport,
                    window: VoiceWindow {
                        start: Beat::new(64.0),
                        duration: None,
                    },
                    file_sample_rate: SampleRate(44100.0),
                },
            );

            // `tick`: pre-dirty the caller's frame so a missing write shows.
            let mut out = vec![9.0f32; width];
            voice.tick(&[], &mut out);
            for (c, &s) in out.iter().enumerate() {
                assert_eq!(
                    s, 0.0,
                    "width {width} tick: channel {c} not silenced outside the window"
                );
            }

            // `process`: same, through the planar path.
            let input = BufferVec::new(0);
            let mut output = BufferVec::new(width);
            {
                let mut buf = output.buffer_mut();
                for c in 0..width {
                    for i in 0..8 {
                        buf.set_f32(c, i, 9.0);
                    }
                }
            }
            voice.process(8, &input.buffer_ref(), &mut output.buffer_mut());
            let buf = output.buffer_ref();
            for c in 0..width {
                for i in 0..8 {
                    assert_eq!(
                        buf.at_f32(c, i),
                        0.0,
                        "width {width} process: channel {c} sample {i} not silenced"
                    );
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
            unit.outputs(),
            width,
            "the cached stride must agree with the declared layout"
        );

        // The block reads frames 0..64, the ring's own.
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(width);
        unit.process(64, &input.buffer_ref(), &mut output.buffer_mut());

        let buf = output.buffer_ref();
        // The last quarter of the block is well past priming.
        for i in 48..64 {
            for c in 0..width {
                let got = buf.at_f32(c, i);
                let want = (c + 1) as f32;
                assert!(
                    (got - want).abs() < 1e-4,
                    "sample {i} channel {c}: read {got}, want {want} — \
                     a wrong stride reads a neighbouring channel's tap"
                );
            }
        }

        // `tick` reads through the same path, so it must land on the same
        // frame.
        let mut frame = vec![0.0f32; width];
        for _ in 0..8 {
            unit.tick(&[], &mut frame);
        }
        for (c, &s) in frame.iter().enumerate() {
            let want = (c + 1) as f32;
            assert!(
                (s - want).abs() < 1e-4,
                "tick channel {c}: read {s}, want {want}"
            );
        }
    }

    /// **A gain change must reach a voice that is already rendering.**
    ///
    /// The clone half of the live-value rule, at the disk tier. `Net`'s
    /// frontend holds clones of its vertices, so a gain stored **by value** in
    /// `DiskSource` is written on one copy and rendered from another — the
    /// authored value silently stops having any effect once the voice exists.
    /// `tutti_nodes`' crate docs state the rule; this pins it for the tier that
    /// broke it.
    ///
    /// Asserted through a **clone**, not through the original, because that is
    /// the only way the two storage conventions differ: a by-value field looks
    /// perfect until something clones the unit, which `Net::commit` does to
    /// every node on every graph edit.
    #[test]
    fn a_gain_change_reaches_a_cloned_voice() {
        let (mut unit, _state) = make_unit(&[(1.0, 1.0); 256]);
        unit.set_sample_rate(SampleRate(48_000.0));
        unit.play();

        // The clone stands in for the copy `Net::commit` hands the audio
        // thread; the original stands in for the frontend the app writes to.
        let mut rendering = unit.clone();

        unit.set_gain(Amplitude::new(0.25));

        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        let mut rendered = 0.0f32;
        for _ in 0..4 {
            rendering.process(8, &input.buffer_ref(), &mut output.buffer_mut());
            for i in 0..8 {
                let v = output.buffer_ref().at_f32(0, i).abs();
                if v > rendered {
                    rendered = v;
                }
            }
        }

        assert!(
            (rendered - 0.25).abs() < 1e-4,
            "a gain written on one copy of the voice must be seen by the copy \
             that renders; expected ~0.25, got {rendered}. A value near 1.0 \
             means `gain` is still stored by value and the write went nowhere."
        );
    }
}
