//! In-memory sample playback with optional loop crossfade.
//!
//! The resident half of the sampler's two playback tiers: a whole `Arc<Wave>` in
//! RAM, indexed at a fractional position, against the disk tier's ring-fed
//! stream. The two share their interpolation kernel and their transport
//! placement gate (`super::interp`) so the same file cannot sound different
//! depending on which tier loaded it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tutti_core::{
    Amplitude, AtomicSamplePosition, AudioUnit, Beat, BeatDuration, BufferMut, BufferRef,
    ChannelLayout, Param, PlaybackRate, ReadRate, SamplePosition, SampleRate, Samples, SignalFrame,
    SrcRatio, Timeline,
};
use tutti_io::Wave;

use super::interp::{read_looped_frame, Seat};
use super::loop_span::LoopSpan;
use super::types::Direction;
use crate::{nonempty, MAX_SAMPLER_CHANNELS};

/// Live loop state on a `MemorySource`. Internal: the public loop *intent* is
/// [`LoopSetting`].
///
/// Modeled on the butler's `Link.loop_config`. One enum rather than three
/// fields: `OneShot` renders `range`/`crossfade_frames` unreachable and
/// `Looping` guarantees a range, so a loop with no range — or a range with
/// looping off — is unrepresentable rather than merely unlikely.
///
/// The crossfade holds no buffer: a loop reads its fade from the wave in place
/// (`LoopSpan`, the one definition of what a looped voice plays), so a loop
/// change is a store, allocation-free on the audio thread's command drain.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) enum LoopMode {
    /// Play through once, then stop.
    #[default]
    OneShot,
    /// Loop over `range` (start, end) in samples, fading over
    /// `crossfade_frames` into it (0 = a hard loop).
    Looping {
        range: (SamplePosition, SamplePosition),
        crossfade_frames: usize,
    },
}

impl LoopMode {
    /// The loop as the read plays it on a wave `len` frames long, or `None`
    /// when one-shot (or a range with nothing in it once clamped to the wave).
    fn span(&self, len: usize) -> Option<LoopSpan> {
        match *self {
            Self::OneShot => None,
            Self::Looping {
                range: (start, end),
                crossfade_frames,
            } => LoopSpan::from_setting(
                LoopSetting::On {
                    start,
                    end,
                    crossfade_frames,
                },
                len,
            ),
        }
    }
}

/// Public loop *intent* for a [`MemorySourceConfig`] — a plain, buildable value
/// that says whether and how to loop. [`MemorySource::with_config`] converts it
/// into the internal loop mode.
///
/// Mirrors how the streaming/timeline loop speaks in
/// `(start, end, crossfade_frames)` via `VoiceCommand::UpdateLoop`, so the same
/// intent crosses both tiers unchanged.
// `Copy`: every field is (`SamplePosition`, `usize`), and the RT command drain
// passes this by value twice per loop update. Without `Copy` those are `.clone()`
// calls that read as heap traffic on the audio thread when they are memcpys.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum LoopSetting {
    /// Play through once, then stop.
    #[default]
    Off,
    /// Loop over `[start, end)` in samples, with `crossfade_frames` of loop
    /// crossfade (0 = hard loop, no crossfade).
    ///
    /// **The fade actually used.** The points are whole frames (truncated),
    /// and `end` is clamped to the file. The fade is linear, over the last
    /// `crossfade_frames` before `end`, and continuous at the wrap:
    ///
    /// - With at least `crossfade_frames` before `start`, the tail blends
    ///   toward the frames leading into `start`, and the wrap lands on
    ///   `start`. The fade is at most the loop's length.
    /// - With fewer (a loop from frame 0, say), the tail blends toward the
    ///   loop's own head `[start, start + fade)` and the wrap lands on
    ///   `start + fade`: the loop that repeats is `[start + fade, end)`. The
    ///   fade is at most half the loop's length there.
    ///
    /// Every tier reads the same rule.
    ///
    /// **When a change is heard.** On the memory tier a change is a store,
    /// heard at the next frame. On a live disk stream (`Command::Loop`) the
    /// butler rewrites the ring it has read ahead, from where the old and new
    /// loops first differ: when that is far enough ahead of the playhead, the
    /// change is heard exactly where the memory tier hears it; when it is at
    /// or near the playhead, it lands about 256 frames past the block being
    /// played (plus up to a butler cycle), crossfaded over the stream's seek
    /// crossfade (`BufferConfig::seek_crossfade_frames`) from what the old
    /// loop would have played. From there on the live voice plays the new
    /// loop exactly as the memory tier does. An export fork taken after the
    /// change reads the new loop from the start of its render (doc 013, "The
    /// live disk loop and its repositions (#48)").
    On {
        start: SamplePosition,
        end: SamplePosition,
        crossfade_frames: usize,
    },
}

/// The span of timeline a voice occupies: where it starts, and how long it
/// lasts.
///
/// **Pure geometry — no clock.** A window and a clock are different kinds of
/// thing: the window is a value a voice owns, while the clock is a shared
/// dependency many voices read. Bundling the `Arc<dyn Timeline>` in here would
/// make every offline rebind reach inside each source to swap one field of a
/// value, and the gate kernel takes the two apart again at every call site
/// anyway.
///
/// Keeping the clock out is what makes this `Copy`: passing a window around
/// clones no `Arc`, and there is no hand-written `Clone`/`Debug` to keep in
/// sync.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceWindow {
    /// Start position in beats on the timeline.
    pub start: Beat,
    /// Duration in beats, or `None` to play the whole source.
    pub duration: Option<BeatDuration>,
}

impl VoiceWindow {
    /// A window starting at `start` and running to the end of the source.
    pub const fn from(start: Beat) -> Self {
        Self {
            start,
            duration: None,
        }
    }

    /// A window of `duration` beats starting at `start`.
    pub const fn span(start: Beat, duration: BeatDuration) -> Self {
        Self {
            start,
            duration: Some(duration),
        }
    }
}

impl Default for VoiceWindow {
    /// From beat 0, for the whole source.
    fn default() -> Self {
        Self {
            start: Beat::new(0.0),
            duration: None,
        }
    }
}

/// Configuration for building a [`MemorySource`], passed to
/// [`MemorySource::with_config`]. Matches tutti's config-struct constructor
/// convention (`PolySynth::new(SynthConfig)`, `OfflineTimeline::new(..)`).
///
/// `Default` yields the same audible baseline as [`MemorySource::new`]: unity
/// gain, normal speed, one-shot, no transport binding. It is hand-written (not
/// derived) because the newtypes default to zero — a derived default would ship
/// silent (`gain = 0`) and frozen (`speed = 0`).
// Hand-rolled `Debug`: `timeline` is an `Arc<dyn Timeline>`, which is not
// `Debug`. Report whether a clock is bound rather than trying to print it — the
// same treatment `MemorySource` itself gets.
#[derive(Clone)]
pub struct MemorySourceConfig {
    /// Linear output gain, applied as one scalar to every channel.
    /// `Amplitude::new(1.0)` is unity — the `Default` value, and not the
    /// newtype's own zero default, which would ship silent.
    pub gain: Amplitude,
    /// Varispeed. Bounded by [`PlaybackRate`]'s own constructor;
    /// [`PlaybackRate::UNITY`] is normal speed and the `Default`. Composes with
    /// the sample-rate conversion through
    /// [`PlaybackRate::read_rate`](tutti_core::PlaybackRate::read_rate), never
    /// by a bare multiply.
    pub speed: PlaybackRate,
    /// Loop intent. `Off` plays once; `On { .. }` loops over the range and
    /// [`MemorySource::with_config`] primes the crossfade internally.
    pub loop_setting: LoopSetting,
    /// Transport clock. `Some` binds this source to a timeline; `None` leaves it
    /// free-running (audition / one-shot).
    pub timeline: Option<Arc<dyn Timeline>>,
    /// Span of timeline the voice occupies. Only consulted when `timeline` is
    /// `Some` — a window without a clock has nothing to be a window *of*.
    pub window: VoiceWindow,
    /// Output width. Defaults to stereo — see [`MemorySource::channels`] for why
    /// this is declared rather than taken from the wave.
    pub channels: ChannelLayout,
}

impl std::fmt::Debug for MemorySourceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySourceConfig")
            .field("gain", &self.gain)
            .field("speed", &self.speed)
            .field("loop_setting", &self.loop_setting)
            .field("placed", &self.timeline.is_some())
            .field("window", &self.window)
            .field("channels", &self.channels)
            .finish()
    }
}

impl Default for MemorySourceConfig {
    fn default() -> Self {
        Self {
            gain: Amplitude::new(1.0),
            speed: PlaybackRate::UNITY,
            loop_setting: LoopSetting::Off,
            timeline: None,
            window: VoiceWindow::default(),
            channels: ChannelLayout::STEREO,
        }
    }
}

/// In-memory sample playback with optional loop crossfade — the resident tier of
/// [`VoiceSource`](super::types::VoiceSource), reading an `Arc<Wave>` by index
/// rather than streaming.
///
/// Plays immediately when added to the graph, which is what timeline voices and
/// offline export want. [`stop`](Self::stop) and [`trigger`](Self::trigger) give
/// manual control for a MIDI-triggered one-shot.
///
/// # Two position models
///
/// A **placed** voice (one with a [`timeline`](Self::timeline)) derives its
/// position from the playhead every frame and cannot drift from the transport; a
/// **free-running** one advances its own cursor by
/// [`read_rate`](Self::read_rate). Which applies decides whether
/// [`position`](Self::position) or [`window_position`](Self::window_position) is
/// the meaningful reading, and which rate the caller must step by.
///
/// # Real-time
///
/// Every playback path is allocation-free and lock-free, including the loop
/// change in [`set_loop_range`](Self::set_loop_range) — the audio thread drains
/// `VoiceCommand::UpdateLoop` inline. The one exception is
/// [`set_wave`](Self::set_wave), which is control-thread only.
pub struct MemorySource {
    wave: Arc<Wave>,
    position: AtomicSamplePosition,

    /// Defaults to true (auto-play).
    playing: AtomicBool,

    /// Linear output gain — **shared across clones**, unlike every other
    /// control field here.
    ///
    /// `Param<Amplitude>` rather than a plain `Amplitude` for the reason
    /// `tutti_nodes`' crate docs give: `Net`'s frontend holds clones, so a
    /// control stored by value is written on one copy and rendered from
    /// another. A clip's fader did nothing once its voice existed.
    ///
    /// Sharing it is what makes [`isolate`](Self::isolate) load-bearing on this
    /// tier — see that method.
    gain: Param<Amplitude>,

    /// Varispeed — user intent, bounded by the type. Composes with
    /// [`src_ratio`](Self::src_ratio) through
    /// [`PlaybackRate::read_rate`](tutti_core::PlaybackRate::read_rate); never
    /// multiplied by a bare scalar.
    speed: PlaybackRate,

    sample_rate: SampleRate,

    /// Sample-rate conversion — derived from the file and session rates, never
    /// user intent. Distinct from [`speed`](Self::speed) for that reason.
    src_ratio: SrcRatio,

    /// Loop configuration. `OneShot` plays through once; `Looping` guarantees a
    /// range and carries the optional crossfade.
    loop_mode: LoopMode,

    /// Transport clock, or `None` for a free-running source.
    ///
    /// Separate from `window` — see [`VoiceWindow`]. `Option` on the CLOCK is
    /// what distinguishes placed from free-running playback; the window is always
    /// present because "from beat 0, whole source" is a meaningful default and
    /// `None` there would mean the same thing twice.
    timeline: Option<Arc<dyn Timeline>>,

    /// Span of timeline this voice occupies. Meaningless without `timeline`.
    window: VoiceWindow,

    /// Whether the free-running cursor has been round its loop: then the frame
    /// behind the loop's start is the loop's last (`LoopSpan::taps`). Atomic
    /// for the reason `position` is: [`trigger`](Self::trigger) clears it
    /// through `&self`.
    looped: AtomicBool,

    /// Where a placed read last seated on the clock, and how far it has run
    /// since. See [`seated_position`](Self::seated_position).
    seat: Option<Seat>,

    /// Output width — this unit's `outputs()`, fixed at construction.
    ///
    /// Deliberately **not** derived from `wave.channels()`: a node whose arity
    /// followed its content would re-arity itself in the graph the moment a
    /// wider file was loaded, and `Net` edges are built against `outputs()`.
    /// The wave's own width is reconciled against this one by
    /// [`read_frame`](super::interp::read_frame)'s channel policy.
    channels: ChannelLayout,
}

// Hand-rolled: `wave` is a non-`Debug` `Arc<Wave>` and `timeline` holds an
// `Arc<dyn Timeline>`. Print the wave length + scalar params; never
// borrow the `Wave` samples.
impl std::fmt::Debug for MemorySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySource")
            .field("wave_frames", &self.wave.len())
            .field("position", &self.position.load(Ordering::Relaxed))
            .field("playing", &self.playing.load(Ordering::Relaxed))
            .field("gain", &self.gain)
            .field("speed", &self.speed)
            .field("sample_rate", &self.sample_rate)
            .field("src_ratio", &self.src_ratio)
            .field("loop_mode", &self.loop_mode)
            .field("placed", &self.timeline.is_some())
            .field("window", &self.window)
            .finish_non_exhaustive()
    }
}

impl Clone for MemorySource {
    fn clone(&self) -> Self {
        Self {
            wave: Arc::clone(&self.wave),
            position: AtomicSamplePosition::new(self.position.load(Ordering::Relaxed)),
            playing: AtomicBool::new(self.playing.load(Ordering::Relaxed)),
            gain: self.gain.handle(),
            speed: self.speed,
            sample_rate: self.sample_rate,
            src_ratio: self.src_ratio,
            loop_mode: self.loop_mode,
            timeline: self.timeline.clone(),
            window: self.window,
            looped: AtomicBool::new(self.looped.load(Ordering::Relaxed)),
            // A copy seats itself from its own clock, which a fork rebinds.
            seat: None,
            channels: self.channels,
        }
    }
}

impl MemorySource {
    /// A **stereo** sampler over `wave`.
    ///
    /// Stays stereo even for a wider wave — see [`channels`](Self::channels).
    /// Use [`with_channels`](Self::with_channels) to declare a different width.
    pub fn new(wave: Arc<Wave>) -> Self {
        let sample_rate = wave.sample_rate();
        Self {
            wave,
            position: AtomicSamplePosition::new(SamplePosition::new(0.0)),
            playing: AtomicBool::new(true),
            gain: Param::new(Amplitude::new(1.0)),
            speed: PlaybackRate::UNITY,
            sample_rate,
            src_ratio: SrcRatio::UNITY,
            loop_mode: LoopMode::OneShot,
            timeline: None,
            window: VoiceWindow::default(),
            looped: AtomicBool::new(false),
            seat: None,
            channels: ChannelLayout::STEREO,
        }
    }

    /// A `channels`-wide sampler over `wave`.
    ///
    /// The wave's own channel count is independent of this; the two are
    /// reconciled per read by [`read_frame`](super::interp::read_frame)'s
    /// channel policy (mono fans, anything else folds).
    pub fn with_channels(wave: Arc<Wave>, channels: impl Into<ChannelLayout>) -> Self {
        Self {
            channels: nonempty(channels.into()),
            ..Self::new(wave)
        }
    }

    /// Output width — this unit's `outputs()`.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// Build from an explicit [`MemorySourceConfig`] — the canonical
    /// configurable constructor, matching tutti's `X::new(XConfig)` convention.
    ///
    /// A `LoopSetting::On { crossfade_frames, .. }` loops with that crossfade
    /// (via [`set_loop_range`](Self::set_loop_range)); `crossfade_frames == 0`
    /// loops with no crossfade.
    pub fn with_config(wave: Arc<Wave>, config: MemorySourceConfig) -> Self {
        let mut unit = Self {
            gain: Param::new(config.gain),
            speed: config.speed,
            timeline: config.timeline,
            window: config.window,
            channels: nonempty(config.channels),
            ..Self::new(wave)
        };
        if let LoopSetting::On {
            start,
            end,
            crossfade_frames,
        } = config.loop_setting
        {
            unit.set_loop_range(start, end, crossfade_frames);
        }
        unit
    }

    /// Convenience constructor for the common transport-bound voice: bind a
    /// clock at `start_beat` for `duration_beats`, everything else default.
    ///
    /// Equivalent to `with_config(wave, MemorySourceConfig { timeline: Some(..),
    /// window, ..Default::default() })`, and it exists because it reads better
    /// at the timeline call sites — the same reason tutti-polysynth keeps
    /// convenience constructors alongside its config one. `duration_beats` of
    /// `None` plays the whole source.
    pub fn with_transport(
        wave: Arc<Wave>,
        transport: Arc<dyn Timeline>,
        start_beat: Beat,
        duration_beats: Option<BeatDuration>,
    ) -> Self {
        Self::with_config(
            wave,
            MemorySourceConfig {
                timeline: Some(transport),
                window: VoiceWindow {
                    start: start_beat,
                    duration: duration_beats,
                },
                ..Default::default()
            },
        )
    }

    /// Move the window. Independent of whether a clock is bound — a window is
    /// just geometry, so there is no "only if placed" branch to get wrong.
    pub fn set_window(&mut self, window: VoiceWindow) {
        self.window = window;
        // The seat's origin was the old window's; the next frame re-seats.
        self.seat = None;
    }

    /// Swap the transport clock, used by export to inject the offline timeline.
    ///
    /// The window is untouched, because it is not part of the same value — see
    /// [`VoiceWindow`]. A swap that also had to reconstruct start/duration would
    /// silently rewrite geometry on the unbound path; keeping the two apart
    /// makes this swap a swap.
    pub fn replace_transport(&mut self, transport: Arc<dyn Timeline>) {
        self.timeline = Some(transport);
        self.seat = None;
    }

    /// Rewind to sample 0 and start playing. Two relaxed atomic stores, so this
    /// is safe from the audio thread.
    ///
    /// Only affects the **free-running** path: a placed voice derives its
    /// position from the transport, so triggering one changes nothing audible.
    pub fn trigger(&self) {
        self.trigger_at(SamplePosition::new(0.0));
    }

    /// [`trigger`](Self::trigger) from an arbitrary [`SamplePosition`] in source
    /// samples rather than from 0.
    pub fn trigger_at(&self, position: SamplePosition) {
        self.position.store(position, Ordering::Relaxed);
        self.looped.store(false, Ordering::Relaxed);
        self.playing.store(true, Ordering::Relaxed);
    }

    /// Resume from wherever the cursor sits, without rewinding. Free-running
    /// path only, as with [`trigger`](Self::trigger).
    pub fn play(&self) {
        self.playing.store(true, Ordering::Relaxed);
    }

    /// Stop and hold the cursor where it is. Subsequent frames are silence until
    /// [`play`](Self::play) or [`trigger`](Self::trigger).
    pub fn stop(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    /// Whether the free-running cursor is advancing. A placed voice ignores this
    /// flag entirely — its silence comes from the window gate, not from here.
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Toggle looping. Enabling loops over the whole sample (unless a range was
    /// already set); disabling drops any range and crossfade.
    pub fn set_looping(&mut self, looping: bool) {
        match (looping, &self.loop_mode) {
            // Already looping — keep the existing range/crossfade.
            (true, LoopMode::Looping { .. }) => {}
            (true, LoopMode::OneShot) => {
                self.loop_mode = LoopMode::Looping {
                    range: (
                        SamplePosition::new(0.0),
                        SamplePosition::new(self.wave.len() as f64),
                    ),
                    crossfade_frames: 0,
                };
                self.looped.store(false, Ordering::Relaxed);
            }
            (false, _) => self.clear_loop_range(),
        }
    }

    /// Whether a loop range is set. Says nothing about a crossfade — a loop may
    /// be hard (0 crossfade frames).
    pub fn is_looping(&self) -> bool {
        matches!(self.loop_mode, LoopMode::Looping { .. })
    }

    /// The free-running cursor, in fractional source samples from the start of
    /// the wave.
    ///
    /// Meaningful only for an unplaced voice. A placed one derives its position
    /// from the playhead each frame and never writes this — read
    /// [`window_position`](Self::window_position) instead.
    pub fn position(&self) -> SamplePosition {
        self.position.load(Ordering::Relaxed)
    }

    /// The transport clock this source reads, or `None` if free-running.
    pub fn timeline(&self) -> Option<Arc<dyn Timeline>> {
        self.timeline.clone()
    }

    /// The voice's window on the timeline. Always meaningful — see
    /// [`VoiceWindow`] for why the window is not itself optional.
    pub fn window(&self) -> VoiceWindow {
        self.window
    }

    /// Where on the timeline this voice's window opens, in [`Beat`]s.
    pub fn start_beat(&self) -> Beat {
        self.window.start
    }

    /// How long the window stays open, in [`BeatDuration`], or `None` to play
    /// the whole source. The end is **exclusive**: at `start + duration` the
    /// gate already reports silence.
    pub fn duration_beats(&self) -> Option<BeatDuration> {
        self.window.duration
    }

    /// Length of the loaded wave in **frames** — one per output sample at unity
    /// read rate, regardless of the wave's channel count.
    pub fn duration_samples(&self) -> usize {
        self.wave.len()
    }

    /// Length of the loaded wave in seconds at the *file's* own sample rate,
    /// which is not the session's unless [`src_ratio`](Self::src_ratio) is
    /// unity. `f64` because a long render's duration outruns `Seconds`' f32.
    pub fn duration_seconds(&self) -> f64 {
        self.wave.duration()
    }

    /// Publish a new output gain.
    ///
    /// `&self` and stored in a shared cell: a value stored by value here is
    /// written on a frontend clone and rendered from a different one. See the
    /// field's doc.
    pub fn set_gain(&self, gain: Amplitude) {
        self.gain.store(gain);
    }

    /// The current output gain, read from the shared cell — so a clone reports
    /// what the frontend last published, not what it was cloned at.
    pub fn gain(&self) -> Amplitude {
        self.gain.load()
    }

    /// Stop sharing the gain cell with whoever this was cloned from.
    ///
    /// The offline render clones the live net and ticks it on a worker thread
    /// **while the original keeps playing**, so a shared control cell would let
    /// the two fight: a fader move during an export would change the exported
    /// audio. `AudioUnit::isolate` exists to sever exactly this, and sharing the
    /// gain is what gives this tier something to sever — `VoiceSource::isolate`'s
    /// `Memory` arm does nothing else.
    ///
    /// Keeps the *current* value: the render must sound like what it was
    /// isolated at, not snap to unity.
    pub(crate) fn isolate_gain(&mut self) {
        self.gain.detach();
    }

    /// Set varispeed. Out-of-range and non-finite values are handled by
    /// [`PlaybackRate`]'s bounded constructor, not here — that is the point of
    /// the type: both playback tiers get the same range without either having
    /// to remember to clamp.
    pub fn set_speed(&mut self, speed: PlaybackRate) {
        self.speed = speed;
    }

    /// The varispeed this voice was set to — user intent alone, with no
    /// sample-rate conversion folded in.
    pub fn speed(&self) -> PlaybackRate {
        self.speed
    }

    /// The sample-rate conversion factor, `file_rate / session_rate`. Derived
    /// by [`set_session_sample_rate`](Self::set_session_sample_rate), never
    /// authored — [`speed`](Self::speed) is the authored quantity.
    pub fn src_ratio(&self) -> SrcRatio {
        self.src_ratio
    }

    /// Source samples consumed per output sample: varispeed × conversion.
    ///
    /// **The step, on both paths.** A free-running cursor advances by it once
    /// per output sample, and a placed read steps by it from the origin the gate
    /// gives (see `seated_position`): an output frame is
    /// `1 / session_rate` seconds, which is `file_rate / session_rate` file
    /// frames at unit speed. The placement gate's *origin* must NOT use this —
    /// see [`window_rate`](Self::window_rate).
    #[inline]
    pub fn read_rate(&self) -> ReadRate {
        self.speed.read_rate(self.src_ratio)
    }

    /// The rate the placement gate maps elapsed time onto this wave with:
    /// varispeed alone, because [`window_position`](Self::window_position)
    /// measures elapsed seconds in this wave's own frames, so the conversion
    /// is complete before any ratio applies.
    ///
    /// **The origin only, never the step.** Within the clock's move a read steps
    /// by output frames, and an output frame is `src_ratio` file frames at unit
    /// speed: stepping by this instead reads a 24 kHz file on a 48 kHz clock at
    /// one file frame per output frame, then jumps back at every block (frames
    /// 60…63, then 32, at 64-frame blocks). The two agree only at matched rates,
    /// where `src_ratio` is exactly 1.
    #[inline]
    pub fn window_rate(&self) -> ReadRate {
        self.speed.read_rate(SrcRatio::UNITY)
    }

    /// Replace the wave data. Resets playback position to the start.
    ///
    /// Call from `graph_mut` — not safe to call from the audio thread directly.
    pub fn set_wave(&mut self, wave: Arc<Wave>) {
        self.sample_rate = wave.sample_rate();
        self.wave = wave;
        self.position
            .store(SamplePosition::new(0.0), Ordering::Release);
        self.looped.store(false, Ordering::Relaxed);
        self.seat = None;
    }

    /// Re-derive the sample-rate conversion ratio for a new session rate.
    ///
    /// The derivation itself lives in [`SrcRatio::for_rates`] — the butler's
    /// streaming path calls the same function, so the two tiers cannot drift
    /// apart on matched-rate detection or the divide-by-zero guard.
    pub fn set_session_sample_rate(&mut self, session_rate: impl Into<SampleRate>) {
        self.src_ratio = SrcRatio::for_rates(self.wave.sample_rate(), session_rate);
    }

    /// Apply a [`LoopSetting`]: `On` primes the range + crossfade, `Off`
    /// clears the range and disables looping.
    ///
    /// In-memory only. The streaming tier's loop is butler-owned (a
    /// `Command::Loop` from the reader's drain), which is why this is inherent
    /// rather than a shared trait method — there is no honest way for one call
    /// to mean both.
    pub fn set_loop_setting(&mut self, setting: LoopSetting) {
        match setting {
            LoopSetting::On {
                start,
                end,
                crossfade_frames,
            } => self.set_loop_range(start, end, crossfade_frames),
            LoopSetting::Off => {
                self.clear_loop_range();
                self.set_looping(false);
            }
        }
    }

    /// Loop over `[loop_start, loop_end)` in source samples, with
    /// `crossfade_frames` **frames** of loop crossfade (0 = a hard loop).
    ///
    /// The loop plays whole frames (its points are truncated, as the butler
    /// takes a stream's), and the fade leads into the loop's start: the last
    /// `crossfade_frames` before the end blend toward the frames *before* the
    /// start, so the wrap continues seamlessly there — or, with too little
    /// before the start, toward the loop's own head, the wrap then resuming
    /// after it ([`LoopSetting::On`] has the rule). The whole rule is
    /// `LoopSpan`'s, which every tier reads through.
    ///
    /// **Allocation-free, and it has to be**: `VoiceCommand::UpdateLoop` is
    /// drained by `VoicePool::drain_commands`, which `tick`/`process` call — so
    /// this runs inside the audio callback. It is a store: the fade is read from
    /// the wave in place, never copied.
    pub fn set_loop_range(
        &mut self,
        loop_start: SamplePosition,
        loop_end: SamplePosition,
        crossfade_frames: usize,
    ) {
        self.loop_mode = LoopMode::Looping {
            range: (loop_start, loop_end),
            crossfade_frames,
        };
        // The cursor has not been round *this* loop. (The taps would read the
        // right frames either way — they only wrap back for a position on the
        // loop — but the flag belongs to the loop it was set by.)
        self.looped.store(false, Ordering::Relaxed);
    }

    /// Drop the loop range and revert to one-shot playback. Allocation-free, so
    /// it is safe on the same audio-thread command drain
    /// [`set_loop_range`](Self::set_loop_range) runs on.
    pub fn clear_loop_range(&mut self) {
        self.loop_mode = LoopMode::OneShot;
        self.looped.store(false, Ordering::Relaxed);
    }

    /// The loop's `(start, end)` in source samples, or `None` when one-shot.
    /// The end is exclusive. Carries no crossfade length — for that, read
    /// [`loop_setting`](Self::loop_setting).
    pub fn loop_range(&self) -> Option<(SamplePosition, SamplePosition)> {
        match &self.loop_mode {
            LoopMode::Looping { range, .. } => Some(*range),
            LoopMode::OneShot => None,
        }
    }

    /// The current loop as a public [`LoopSetting`] intent, with the crossfade
    /// length in **frames** as it was asked for (0 for a hard loop).
    ///
    /// Lets a caller that built this unit imperatively read its loop back as a
    /// value — the `VoicePool` add shim uses it to fold a pre-configured
    /// `MemorySource`'s loop into a `Playback` record.
    pub fn loop_setting(&self) -> LoopSetting {
        match &self.loop_mode {
            LoopMode::Looping {
                range,
                crossfade_frames,
            } => LoopSetting::On {
                start: range.0,
                end: range.1,
                crossfade_frames: *crossfade_frames,
            },
            LoopMode::OneShot => LoopSetting::Off,
        }
    }

    /// Read one un-gained frame into `out` (width = `out.len()`).
    ///
    /// Writes every element, so the caller never pre-zeros. The wave's own
    /// channel count need not match `out.len()` —
    /// [`read_frame`](super::interp::read_frame) owns that policy.
    #[inline]
    pub fn get_sample_raw_into(&self, position: f64, out: &mut [f32]) {
        let len = self.wave.len() as f64;
        if position >= len {
            out.fill(0.0);
            return;
        }

        // 4-tap cubic Hermite via the shared kernel (idx-1, idx, idx+1, idx+2,
        // bound-clamped), unifying this path with `DiskSource`.
        super::interp::read_frame(&self.wave, position, out);
    }

    /// Read one un-gained frame of a **placed** voice at `pos`, the position
    /// the gate and the step give (see [`seated_position`](Self::seated_position)),
    /// into `out`, writing every element.
    ///
    /// - **Forward**, on a loop: a position at or past the loop's end plays
    ///   inside it, and the loop's fade and seam are read as `LoopSpan` has
    ///   them, as the offline disk reader reads the same stream's loop.
    /// - **Reverse** mirrors the position about the last frame (`len - 1 -
    ///   pos`) and ignores the loop, as the butler's reverse refill does. It is
    ///   silent where a forward read would be, at and past `len`: the mirror
    ///   of a read past the end is a read before the start, and it must fall
    ///   silent rather than hold frame 0 as DC. Between the two (`pos` in
    ///   `(len - 1, len)`) the mirror sits under a frame before the first and
    ///   reads frame 0, as a forward read in `[len - 1, len)` reads the last
    ///   frame.
    #[inline]
    pub(crate) fn read_placed_into(
        &self,
        pos: SamplePosition,
        direction: Direction,
        out: &mut [f32],
    ) {
        let len = self.wave.len() as f64;
        match direction {
            Direction::Reverse => {
                if pos.get() >= len {
                    out.fill(0.0);
                    return;
                }
                self.get_sample_raw_into((len - 1.0 - pos.get()).max(0.0), out);
            }
            Direction::Forward => match self.loop_mode.span(self.wave.len()) {
                Some(span) => {
                    let (p, looped) = span.place(pos.get());
                    self.read_looped_raw_into(&span, p, looped, out);
                }
                None => self.get_sample_raw_into(pos.get(), out),
            },
        }
    }

    /// One un-gained frame of this wave on `span`, at `p` placed on it: silent
    /// at and past the file's end, as an unlooped read is.
    #[inline]
    fn read_looped_raw_into(&self, span: &LoopSpan, p: f64, looped: bool, out: &mut [f32]) {
        if p >= self.wave.len() as f64 {
            out.fill(0.0);
            return;
        }
        read_looped_frame(&self.wave, span, p, looped, out);
    }

    /// The position a placed read plays this frame, or `None` outside its
    /// window (or unplaced).
    ///
    /// **Seated from the clock, stepped by the read rate.** The clock moves
    /// between calls (per block, or per 64-frame chunk), not per frame, and a
    /// voice may be read a frame at a time through `tick`. So the read seats
    /// where the gate puts the playhead whenever the clock reads a beat it did
    /// not read last time, and steps from there: frame `n` of a seat is
    /// `origin + read_rate × stretch_rate × n`. The origin comes from
    /// [`stretched_window_position`](Self::stretched_window_position) (varispeed
    /// and the stretch, measured in this wave's frames), the step from
    /// [`read_rate`](Self::read_rate) (varispeed and the conversion) — the one
    /// model the offline disk reader seats by too, so the tiers read the same
    /// positions, frame for frame.
    ///
    /// `process` and `tick` both come through here, once per output frame: a
    /// `tick` against a clock that moves once per block steps through the block
    /// as `process` does, rather than repeat one frame. Pass
    /// [`ReadRate::UNITY`] when nothing stretches.
    #[inline]
    pub(crate) fn seated_position(&mut self, stretch_rate: ReadRate) -> Option<SamplePosition> {
        let timeline = self.timeline.as_ref()?;
        let rate = self.read_rate().then(stretch_rate);
        let seat = Seat::next(self.seat, timeline.as_ref(), rate, || {
            self.stretched_window_position(stretch_rate)
        });
        self.seat = seat;
        seat.map(|seat| seat.position())
    }

    /// [`get_sample_raw_into`](Self::get_sample_raw_into) with this unit's gain
    /// applied. One scalar gain across every channel — per-channel level is the
    /// mixer strip's job, not the reader's.
    #[inline]
    pub fn get_sample_into(&self, position: f64, out: &mut [f32]) {
        self.get_sample_raw_into(position, out);
        let gain = self.gain.load().get();
        for s in out.iter_mut() {
            *s *= gain;
        }
    }

    /// Where the playhead sits in this source's samples, or `None` when outside
    /// the window / unplaced. See [`window_position`](super::interp::window_position).
    ///
    /// # Varispeed alone, never [`read_rate`](Self::read_rate)
    ///
    /// The gate maps wall-clock seconds onto *this wave's own* samples, and
    /// `wave.sample_rate()` is already that wave's rate — so the beat→sample
    /// conversion is complete before any ratio is applied. `read_rate` folds in
    /// `src_ratio` (`file_rate / session_rate`), which belongs to the
    /// *free-running* path, where a cursor steps through file samples once per
    /// output sample and genuinely needs both factors.
    ///
    /// Passing it here multiplies the derived position by `src_ratio` a second
    /// time: a 48 kHz file in a 44.1 kHz session reads 104,490 samples in at the
    /// two-second mark instead of 96,000 — 8.8% deep, drifting further the longer
    /// the voice plays. `set_sample_rate` seeds `src_ratio` from the live graph,
    /// so that reaches every placed voice whose file rate differs from the
    /// session's, and stays invisible at matched rates where `src_ratio` is
    /// `UNITY` and the extra factor is exactly 1.0.
    ///
    /// The disk tier reaches the same product by the mirror-image split. Two
    /// splits, one product — `both_tier_splits_agree_on_the_same_position` pins
    /// the agreement.
    #[inline]
    pub fn window_position(&self) -> Option<SamplePosition> {
        let timeline = self.timeline.as_ref()?;
        super::interp::window_position(
            timeline.as_ref(),
            self.window.start,
            self.window.duration,
            self.wave.sample_rate(),
            self.window_rate(),
        )
    }

    /// [`window_position`](Self::window_position) for a **stretched** read, where
    /// the source is consumed at `stretch_rate` rather than at wall-clock rate.
    ///
    /// The rate must reach the *origin*, not only the within-block step, and that
    /// is the whole reason this exists. `window_position` re-derives its origin
    /// from the playhead on every block, and the playhead advances at wall clock.
    /// A caller that seats itself there and then steps by a stretched rate gets a
    /// sawtooth: block N covers `block_size / stretch` source samples, but block
    /// N+1 re-seats a full `block_size` further on, discarding the difference. At
    /// 2x the read jumps forward 32 samples every 64, forever.
    ///
    /// Measured, with the rate reaching neither origin nor step: a placed voice
    /// at 2.0x emits 880 Hz from a 440 Hz source with its duration unchanged —
    /// the stretch factor acting as pure varispeed. Folding the rate into the
    /// step *alone* is worse, not better (pitch 35% off, spectral purity 0.95
    /// against 0.54), because then the two disagree within every block as well
    /// as across them.
    ///
    /// Separate from `window_position` rather than folded into it: that method's
    /// varispeed-only contract is load-bearing for the disk tier and for the
    /// unstretched memory path, both of which still call it. Two named methods,
    /// each with one meaning.
    #[inline]
    pub fn stretched_window_position(&self, stretch_rate: ReadRate) -> Option<SamplePosition> {
        let timeline = self.timeline.as_ref()?;
        super::interp::window_position(
            timeline.as_ref(),
            self.window.start,
            self.window.duration,
            self.wave.sample_rate(),
            self.window_rate().then(stretch_rate),
        )
    }

    /// Produce one output frame and advance whatever state that entails.
    ///
    /// # One algorithm, two entry points
    ///
    /// The single playback algorithm. `tick` calls it once, `process` calls it
    /// per sample — the same relationship `TransportClock::tick`/`process` have
    /// in `tutti-core`. Writing the two entry points out separately is what lets
    /// them drift: a `tick` that dropped `speed` on the placed path while
    /// `process` applied it made the same unit produce different audio depending
    /// on which one the graph happened to call.
    ///
    /// # Two position models
    ///
    /// The split is deliberate:
    ///
    /// - **Placed** (a timeline clip) — position is *derived* from the playhead,
    ///   so the voice cannot drift from the transport: seated where the gate
    ///   puts the playhead and stepped by the read rate until the clock moves
    ///   ([`seated_position`](Self::seated_position)). A transport advances
    ///   once per *block*, not per sample — the offline driver calls
    ///   `advance(block_size)` after `process` returns, and `TransportClock` is
    ///   emit-then-advance — so re-deriving from `beat()` alone would emit one
    ///   constant frame all block long.
    /// - **Free-running** (no transport) — nothing else owns this voice's time,
    ///   so it advances its own cursor by `read_rate`.
    ///
    /// Writes every element of `out` on every path, so a caller never pre-zeros
    /// and a partial write can never leave a stale channel from the previous
    /// block in a trailing slot.
    #[inline]
    fn next_frame_into(&mut self, out: &mut [f32]) {
        if self.timeline.is_some() {
            match self.seated_position(ReadRate::UNITY) {
                None => out.fill(0.0),
                Some(pos) => {
                    self.read_placed_into(pos, Direction::Forward, out);
                    self.apply_gain(out);
                }
            }
            return;
        }

        if !self.playing.load(Ordering::Relaxed) {
            out.fill(0.0);
            return;
        }

        let pos = self.position.load(Ordering::Relaxed).get();
        let wave_len = self.wave.len() as f64;
        let span = self.loop_mode.span(self.wave.len());
        match &span {
            Some(span) => {
                let looped = self.looped.load(Ordering::Relaxed);
                self.read_looped_raw_into(span, pos, looped, out);
                self.apply_gain(out);
            }
            None => self.get_sample_into(pos, out),
        }

        // One output sample's worth of source material.
        let new_pos = pos + self.read_rate().advance(Samples(1)).get();

        // A range with nothing in it keeps its old meaning: the cursor pins to
        // its start (`wrap_into_loop`), rather than play on as one-shot.
        let (looping, loop_start, loop_end) = match (&span, self.loop_mode) {
            (Some(span), _) => (true, span.resume() as f64, span.end() as f64),
            (None, LoopMode::Looping { range, .. }) => (true, range.0.get(), range.1.get()),
            (None, LoopMode::OneShot) => (false, 0.0, wave_len),
        };

        if new_pos >= loop_end {
            if looping {
                // Modulo, not `loop_start + (new_pos - loop_end)`: at high
                // varispeed one advance can overshoot a short loop by more than
                // its own length, and the subtraction form would land past the
                // loop end and never recover. A loop lands on its `resume`
                // (`LoopSpan::place`), after its head when the fade went there.
                let wrapped = match &span {
                    Some(span) => span.place(new_pos).0,
                    None => wrap_into_loop(new_pos, loop_start, loop_end),
                };
                self.position
                    .store(SamplePosition::new(wrapped), Ordering::Relaxed);
                self.looped.store(true, Ordering::Relaxed);
            } else {
                self.playing.store(false, Ordering::Relaxed);
                self.position
                    .store(SamplePosition::new(loop_end), Ordering::Relaxed);
            }
        } else {
            self.position
                .store(SamplePosition::new(new_pos), Ordering::Relaxed);
        }
    }

    /// Scale a frame by this unit's gain.
    #[inline]
    fn apply_gain(&self, out: &mut [f32]) {
        let gain = self.gain.load().get();
        for s in out.iter_mut() {
            *s *= gain;
        }
    }
}

/// Wrap a position that ran past `loop_end` back into `[loop_start, loop_end)`.
///
/// Modulo rather than a single subtraction so an overshoot larger than the loop
/// itself still lands inside the region — reachable at high varispeed over a
/// short loop. A zero-or-negative-length region has nothing to wrap into, so the
/// position pins to `loop_start` rather than producing NaN.
///
/// Deliberately NOT shared with the butler's `wave_io::wrap_into`: that one is
/// integer-domain over a range the caller has already validated, while this
/// reads a fractional position and must survive a degenerate range. Unifying
/// them would put a lossy cast on the per-sample read path.
#[inline]
pub(crate) fn wrap_into_loop(pos: f64, loop_start: f64, loop_end: f64) -> f64 {
    let len = loop_end - loop_start;
    if len <= 0.0 {
        return loop_start;
    }
    loop_start + (pos - loop_start).rem_euclid(len)
}

impl AudioUnit for MemorySource {
    /// Stop sharing the gain cell with whoever this was cloned from.
    ///
    /// Implemented here rather than only in `VoiceSource::isolate` so the
    /// severing happens wherever a unit is isolated — the offline render's
    /// isolation pass walks *every node of the cloned net*, and a source
    /// reached that way would otherwise keep following the live fader.
    fn isolate(&mut self) {
        self.isolate_gain();
    }

    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        // Boundary: `AudioUnit::outputs` is a fixed fundsp trait signature.
        self.channels.count() as usize
    }

    fn reset(&mut self) {
        self.position
            .store(SamplePosition::new(0.0), Ordering::Relaxed);
        self.playing.store(false, Ordering::Relaxed);
        self.looped.store(false, Ordering::Relaxed);
        self.seat = None;
    }

    /// Re-point this source's own read clock at the render's transport.
    ///
    /// A `MemorySource` reaches the graph two ways: wrapped in a `VoicePool` /
    /// `VoiceNode` (which cascade into it), and — since it is itself an
    /// `AudioUnit` — directly as a node. A rebind that knows only the wrappers
    /// leaves a bare memory source rendering against the live playhead.
    /// Declaring it here covers both routes, and any future one.
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        let Some(transport) = ctx.downcast_ref::<tutti_core::transport::OfflineTransport>() else {
            return;
        };
        self.replace_transport(transport.clone());
    }

    fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.sample_rate = sample_rate;
        self.set_session_sample_rate(sample_rate.get());
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        // The caller's slice IS the frame — no intermediate storage needed.
        // Stride derived once — `next_frame_into` is the loop.
        let n = (self.channels.count() as usize).min(output.len());
        self.next_frame_into(&mut output[..n]);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        // `BufferMut` is planar `(channel, index)` with no frame-shaped
        // accessor, so unlike `tick` this one genuinely needs a frame to
        // scatter from. Stack-allocated at the fixed ceiling and used as a
        // prefix — the house pattern (see `tutti-export`'s `fold_graph_frame` and
        // the plugin hosts), and the only way to stay alloc-free at a runtime
        // width.
        // Stride derived once per block, above the loops.
        let n = (self.channels.count() as usize)
            .min(output.channels())
            .min(MAX_SAMPLER_CHANNELS);
        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
        for i in 0..size {
            self.next_frame_into(&mut frame[..n]);
            for (c, &s) in frame.iter().enumerate().take(n) {
                output.set_f32(c, i, s);
            }
        }
    }

    audio_unit_boilerplate!(id = crate::node_id::SAMPLER_NODE_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        // Boundary: `SignalFrame::new` is a fundsp signature.
        SignalFrame::new(self.channels.count() as usize)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::{Bpm, BufferVec};

    fn ramp_wave(len: usize, sample_rate: f64) -> Arc<Wave> {
        let samples: Vec<f32> = (0..len).map(|i| (i + 1) as f32).collect();
        Arc::new(Wave::from_samples(sample_rate, &samples))
    }

    fn stereo_ramp_wave(len: usize, sample_rate: f64) -> Arc<Wave> {
        let mut wave = Wave::zero(2, sample_rate, len as f64 / sample_rate);
        for i in 0..len {
            wave.set(0, i, (i + 1) as f32);
            wave.set(1, i, -((i + 1) as f32));
        }
        Arc::new(wave)
    }

    use crate::test_transport::MockTransport;

    // --- tick/process equivalence ---
    //
    // `tick` and `process` are two entry points into one algorithm, so N ticks
    // must equal one process(N) sample-for-sample. Modelled on tutti-core's
    // `advance_wraps_once_per_block_not_once_per_sample`.
    //
    // KNOWN LIMIT: these are CONSISTENCY checks, not correctness ones. Both
    // paths call `next_frame`, so a change moves them together — an injected
    // off-by-one in the placed branch still passes here, because advancing the
    // mock one sample per tick compensates it exactly. What catches that class
    // of bug is `placed_clip_reads_across_a_block_not_dc`
    // and `placed_clip_block_step_follows_playback_rate`, which assert the
    // shape of the output *within* one block. Keep these as regression guards
    // against the two paths being rewritten apart again; do not read a pass
    // here as proof the placed branch is right.

    fn collect_ticks(unit: &mut MemorySource, n: usize) -> Vec<(f32, f32)> {
        (0..n)
            .map(|_| {
                let mut out = [0.0f32; 2];
                unit.tick(&[], &mut out);
                (out[0], out[1])
            })
            .collect()
    }

    fn collect_process(unit: &mut MemorySource, n: usize) -> Vec<(f32, f32)> {
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(2);
        output.resize(n);
        unit.process(n, &input.buffer_ref(), &mut output.buffer_mut());
        (0..n)
            .map(|i| (output.at_f32(0, i), output.at_f32(1, i)))
            .collect()
    }

    /// `tick` is one sample per call and a transport advances once per *block*,
    /// so the equivalent of `process(n)` is n single-sample blocks with the
    /// playhead moving a sample's worth between each. `transport` must be the
    /// clock both units are bound to; pass `None` for free-running units.
    ///
    /// Advancing matters: with a frozen playhead every placed frame derives the
    /// same position, both paths emit the same constant, and the assertion holds
    /// no matter what the code does.
    fn assert_tick_matches_process(
        mut a: MemorySource,
        mut b: MemorySource,
        n: usize,
        transport: Option<&Arc<MockTransport>>,
        case: &str,
    ) {
        let ticked: Vec<(f32, f32)> = (0..n)
            .map(|_| {
                let mut out = [0.0f32; 2];
                a.tick(&[], &mut out);
                if let Some(t) = transport {
                    t.advance(1, 44100.0);
                }
                (out[0], out[1])
            })
            .collect();

        // Rewind so `process` sees the same span the ticks just walked.
        if let Some(t) = transport {
            t.advance(-(n as i64), 44100.0);
        }
        let processed = collect_process(&mut b, n);

        assert_eq!(
            ticked, processed,
            "{case}: tick x{n} diverged from process({n})"
        );
    }

    #[test]
    fn tick_matches_process_free_running() {
        let wave = ramp_wave(64, 44100.0);
        assert_tick_matches_process(
            MemorySource::new(Arc::clone(&wave)),
            MemorySource::new(wave),
            16,
            None,
            "free-running",
        );
    }

    #[test]
    fn tick_matches_process_at_non_unity_speed() {
        let wave = ramp_wave(256, 44100.0);
        let build = || {
            let mut u = MemorySource::new(Arc::clone(&wave));
            u.set_speed(PlaybackRate::new(1.5));
            u
        };
        assert_tick_matches_process(build(), build(), 32, None, "free-running @1.5x");
    }

    #[test]
    fn tick_matches_process_when_placed() {
        // The case that was actually broken: a placed voice at non-unity speed.
        let wave = ramp_wave(4096, 44100.0);
        let transport = MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
        let build = || {
            let mut u = MemorySource::with_config(
                Arc::clone(&wave),
                MemorySourceConfig {
                    speed: PlaybackRate::new(0.5),
                    timeline: Some(transport.clone()),
                    ..Default::default()
                },
            );
            u.set_speed(PlaybackRate::new(0.5));
            u
        };
        assert_tick_matches_process(build(), build(), 32, Some(&transport), "placed @0.5x");
    }

    #[test]
    fn tick_matches_process_across_a_loop_wrap() {
        // Span the loop boundary so the wrap arithmetic runs inside the block.
        let wave = ramp_wave(64, 44100.0);
        let build = || {
            MemorySource::with_config(
                Arc::clone(&wave),
                MemorySourceConfig {
                    loop_setting: LoopSetting::On {
                        start: SamplePosition::new(0.0),
                        end: SamplePosition::new(8.0),
                        crossfade_frames: 0,
                    },
                    ..Default::default()
                },
            )
        };
        assert_tick_matches_process(build(), build(), 32, None, "loop wrap");
    }

    // --- varispeed actually reaches a placed voice ---
    //
    // The equivalence tests above cannot catch this class on their own: both
    // entry points call `next_frame`, so any change affects them identically.
    // These pin the *behaviour* instead — that speed reaches the placed path at
    // all. A `tick` deriving position without the rate plays a placed voice at
    // 1x no matter what speed was set, and the equivalence pair stays green.

    fn placed_unit(wave: &Arc<Wave>, transport: &Arc<MockTransport>, rate: f32) -> MemorySource {
        MemorySource::with_config(
            Arc::clone(wave),
            MemorySourceConfig {
                speed: PlaybackRate::new(rate),
                timeline: Some(transport.clone()),
                ..Default::default()
            },
        )
    }

    #[test]
    fn placed_clip_honours_speed_in_tick() {
        // A ramp wave encodes position in its amplitude, so the sample value at
        // a fixed playhead names which source frame was read.
        // Beat 0.25 @ 120 BPM / 44.1 kHz = 5512.5 samples in, comfortably
        // inside a 16k wave at both 1x and 0.5x.
        let wave = ramp_wave(16_384, 44100.0);
        let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));

        let mut unity = placed_unit(&wave, &transport, 1.0);
        let mut half = placed_unit(&wave, &transport, 0.5);

        let a = collect_ticks(&mut unity, 1)[0].0;
        let b = collect_ticks(&mut half, 1)[0].0;

        assert!(a > 0.0 && b > 0.0, "both should be sounding: {a}, {b}");
        assert!(
            (b - a / 2.0).abs() < 2.0,
            "at 0.5x the voice should be half as far in ({a} -> expected ~{}, got {b})",
            a / 2.0
        );
    }

    #[test]
    fn placed_clip_honours_speed_in_process() {
        // The same assertion through the block path — this one always held, and
        // is here so the pair documents that the two agree for the right reason.
        let wave = ramp_wave(16_384, 44100.0);
        let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));

        let mut unity = placed_unit(&wave, &transport, 1.0);
        let mut half = placed_unit(&wave, &transport, 0.5);

        let a = collect_process(&mut unity, 4)[0].0;
        let b = collect_process(&mut half, 4)[0].0;

        assert!((b - a / 2.0).abs() < 2.0, "expected ~{}, got {b}", a / 2.0);
    }

    /// A placed voice must read ACROSS a block, not emit one frozen frame.
    ///
    /// A transport advances once per block — the offline driver calls
    /// `advance(block_size)` after `process` returns — so deriving position from
    /// `beat()` alone gives every sample in the block the same value. The result
    /// is constant DC where the material should be moving. Caught only by
    /// asserting *within* one `process` call: the tick/process equivalence tests
    /// cannot see it, because a frozen transport freezes both paths identically.
    #[test]
    fn placed_clip_reads_across_a_block_not_dc() {
        let wave = ramp_wave(16_384, 44100.0);
        let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
        let mut u = placed_unit(&wave, &transport, 1.0);

        let block = collect_process(&mut u, 8);
        let first = block[0].0;
        let last = block[7].0;

        assert!(first > 0.0, "voice should be sounding, got {first}");
        assert!(
            last > first,
            "a ramp wave must rise across the block; got constant DC \
             (first={first}, last={last}) — the transport only moves between \
             blocks, so position must step by read_rate within one"
        );
        // Unity rate over a ramp: one source sample per output sample.
        let step = (last - first) / 7.0;
        assert!(
            (step - 1.0).abs() < 0.01,
            "at 1x the ramp should advance ~1 sample per output frame, got {step}"
        );
    }

    /// Same, at half speed: the block must still rise, but half as fast.
    #[test]
    fn placed_clip_block_step_follows_playback_rate() {
        let wave = ramp_wave(16_384, 44100.0);
        let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
        let mut u = placed_unit(&wave, &transport, 0.5);

        let block = collect_process(&mut u, 8);
        let step = (block[7].0 - block[0].0) / 7.0;
        assert!(
            (step - 0.5).abs() < 0.01,
            "at 0.5x the in-block step should be ~0.5 samples/frame, got {step}"
        );
    }

    /// **A placed wave at another rate than the clock's steps by the
    /// conversion**, across block boundaries, through `process` and through
    /// `tick`: a 24 kHz ramp on a 48 kHz clock reads file frame `n / 2` on
    /// output frame `n`, a monotone read half a frame per frame, with no jump
    /// where a block starts.
    ///
    /// Stepping by the gate's rate (`window_rate`, varispeed alone) read one
    /// file frame per output frame within a block and re-seated half a block
    /// back at the next (frames 60…63, then 32). `tick` against a clock that
    /// moves once per block read one frame all block long.
    ///
    /// Mutation (run): `seated_position`'s step `read_rate` → `window_rate` →
    /// output frame 1 reads file frame 1 → fails (both paths). Mutation (run):
    /// `Seat::next` re-seating on every call (never running on) → `tick`
    /// repeats one frame per block → fails; `process` too, as both come
    /// through the seat.
    #[test]
    fn a_placed_wave_at_another_rate_steps_by_the_conversion() {
        let wave = ramp_wave(4_096, 24_000.0);
        for via_tick in [false, true] {
            let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
            let mut u = placed_unit(&wave, &transport, 1.0);
            u.set_sample_rate(SampleRate::new(48_000.0));
            let mut out = Vec::new();
            for _ in 0..8 {
                if via_tick {
                    out.extend(collect_ticks(&mut u, 64));
                } else {
                    out.extend(collect_process(&mut u, 64));
                }
                transport.advance(64, 48_000.0);
            }
            // From frame 4: the first frames' taps clamp at the file's start.
            for (n, &(l, _)) in out.iter().enumerate().skip(4) {
                let want = n as f32 / 2.0 + 1.0;
                assert!(
                    (l - want).abs() < 1e-3,
                    "tick {via_tick}: output frame {n} read {l}, want file frame {} ({want})",
                    n as f32 / 2.0
                );
            }
        }
    }

    /// **A stretched placed read seats and steps with the stretch, at another
    /// rate too** — the positions the stretched slot branch feeds its filter.
    /// A 24 kHz wave on a 48 kHz clock at a stretch read rate of 0.5: each
    /// block seats where the gate puts it with the stretch (16 file frames per
    /// 64-frame block) and steps a quarter frame per output frame (the
    /// conversion's half, times the stretch's), so the read is one line
    /// across every block.
    ///
    /// Asserted on the positions, not on the filter's output: the phase
    /// vocoder hides a wrong step (a 440 Hz source read at twice the rate
    /// within each block and re-seated at the next still measures 440 Hz), so
    /// a pitch test cannot see this. Whether the slot passes the filter's rate
    /// here at all is `a_stretched_placed_voice_holds_its_pitch_across_blocks`'s.
    ///
    /// Mutation (run): the step `read_rate` → `window_rate` → half a frame per
    /// frame → fails. Mutation (run): the seat by `window_position` (no
    /// stretch) → each block seats at 32 → fails.
    #[test]
    fn a_stretched_placed_read_steps_by_the_conversion_and_the_stretch() {
        let wave = ramp_wave(4_096, 24_000.0);
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let mut u = placed_unit(&wave, &transport, 1.0);
        u.set_sample_rate(SampleRate::new(48_000.0));
        for block in 0..4 {
            for i in 0..64 {
                let pos = u.seated_position(ReadRate(0.5)).expect("inside the window");
                let want = (block * 64 + i) as f64 * 0.25;
                assert!(
                    (pos.get() - want).abs() < 1e-9,
                    "block {block}, frame {i}: position {}, want {want}",
                    pos.get()
                );
            }
            transport.advance(64, 48_000.0);
        }
    }

    /// **The interpolator's taps wrap through the loop** (doc 013 follow-up
    /// N2, the memory tier): at half speed over a hard loop `[10, 20)`, a
    /// position half a frame before the end interpolates frames 18, 19, then
    /// 10, 11 — what the loop plays next — not 20, 21 from past it; and after
    /// the wrap, half a frame into the loop, the frame behind is 19, the one
    /// the loop just played. The ramp's value is its frame + 1.
    ///
    /// Mutation (run): `LoopSpan::taps` clamping to the file rather than
    /// wrapping → reads 20.5 at 19.5 → fails. Mutation (run): the `looped`
    /// back tap removed → frame 9 behind 10.5 → fails. Mutation (run): the
    /// wrap not marking the cursor `looped` → the same → fails.
    #[test]
    fn a_loop_reads_through_its_seam_at_a_fractional_rate() {
        use super::super::interp::cubic_hermite;
        let wave = ramp_wave(64, 44_100.0);
        let mut u = MemorySource::new(wave);
        u.set_loop_range(SamplePosition::new(10.0), SamplePosition::new(20.0), 0);
        u.set_speed(PlaybackRate::new(0.5));
        u.trigger_at(SamplePosition::new(19.5));
        let ticks = collect_ticks(&mut u, 3);
        let v = |frame: usize| (frame + 1) as f32;
        assert_eq!(
            ticks[0].0,
            cubic_hermite(v(18), v(19), v(10), v(11), 0.5),
            "at 19.5"
        );
        // 19.5 + 0.5 = 20.0 wraps to 10.0, which is frame 10 exactly.
        assert_eq!(ticks[1].0, v(10), "at 10.0, after the wrap");
        assert_eq!(
            ticks[2].0,
            cubic_hermite(v(19), v(10), v(11), v(12), 0.5),
            "at 10.5, after the wrap"
        );
    }

    /// **A rate change between two clock moves continues from where the read
    /// stands**: 16 frames at 1×, then 2× on the same clock reading, steps 2
    /// from the last frame read — the seat re-anchors, rather than rescale the
    /// frames it has already stepped (which jumped the read 17 frames).
    ///
    /// Mutation (run): `Seat::next` keeping the seat on a rate change (stepping
    /// `origin + new_rate × frames`) → frame 16 reads 16 frames on → fails.
    #[test]
    fn a_rate_change_mid_seat_continues_where_the_read_stands() {
        let wave = ramp_wave(16_384, 44_100.0);
        let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
        let mut u = placed_unit(&wave, &transport, 1.0);
        let mut out = collect_process(&mut u, 16);
        u.set_speed(PlaybackRate::new(2.0));
        out.extend(collect_process(&mut u, 16));
        for (n, w) in out.windows(2).enumerate() {
            let want = if n < 15 { 1.0 } else { 2.0 };
            assert!(
                (w[1].0 - w[0].0 - want).abs() < 1e-3,
                "frame {} steps {} from frame {n}, want {want}",
                n + 1,
                w[1].0 - w[0].0
            );
        }
    }

    /// **A crossfaded loop plays the hand-computed frames** — the memory
    /// tier's oracle, independent of `LoopSpan`'s arithmetic (the disk fork's
    /// is `a_fork_loops_as_the_stream_is_looped_when_it_is_taken`). A ramp
    /// (value = frame + 1) at unit speed from frame 0, round the loop twice:
    ///
    /// - `[10, 30)`, a 4-frame fade with room before the start: frame `26 + k`
    ///   blends toward `6 + k`, weighing it `(k + 1) / 5`, and the wrap lands
    ///   on 10.
    /// - `[2, 30)`, the same fade with only 2 frames before the start: frame
    ///   `26 + k` blends toward the loop's head `2 + k`, and the wrap lands on
    ///   6, after the head.
    ///
    /// Mutation (run): the weight `k / fade` in `LoopSpan::fade_at` → frame 26
    /// reads the pure tail → fails. Mutation (run): the head mode's `resume`
    /// left at `start` → the second loop wraps to 2 → fails.
    #[test]
    fn a_crossfaded_loop_plays_the_hand_computed_frames() {
        for (start, resume, lead) in [(10usize, 10usize, 6usize), (2, 6, 2)] {
            let mut u = MemorySource::new(ramp_wave(64, 44_100.0));
            u.set_loop_range(
                SamplePosition::new(start as f64),
                SamplePosition::new(30.0),
                4,
            );
            let got: Vec<f32> = collect_ticks(&mut u, 30 + 2 * (30 - resume))
                .iter()
                .map(|f| f.0)
                .collect();
            let frames = (0..30).chain(resume..30).chain(resume..30);
            for (n, (p, &g)) in frames.zip(got.iter()).enumerate() {
                let want = if p >= 26 {
                    let k = p - 26;
                    let t = (k + 1) as f32 / 5.0;
                    (p + 1) as f32 * (1.0 - t) + (lead + k + 1) as f32 * t
                } else {
                    (p + 1) as f32
                };
                assert!(
                    (g - want).abs() < 1e-5,
                    "loop from {start}: output {n} (frame {p}) read {g}, want {want}"
                );
            }
        }
    }

    /// **A stopped clock silences a placed read mid-clip**, through `process`
    /// and through `tick`: the clock stops where it stands (its beat does not
    /// move), and the read must not run on from its seat as if it still
    /// rolled.
    ///
    /// Mutation (run): the `is_rolling` guard removed from `Seat::next` → the
    /// seat runs on through the stop → fails.
    #[test]
    fn a_stopped_clock_silences_a_placed_read() {
        let wave = ramp_wave(16_384, 44_100.0);
        for via_tick in [false, true] {
            let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
            let mut u = placed_unit(&wave, &transport, 1.0);
            let block = |u: &mut MemorySource| {
                if via_tick {
                    collect_ticks(u, 64)
                } else {
                    collect_process(u, 64)
                }
            };
            assert!(
                block(&mut u).iter().all(|&(l, _)| l > 0.0),
                "tick {via_tick}: rolling, the clip plays"
            );
            transport.set_rolling(false);
            assert!(
                block(&mut u).iter().all(|&(l, r)| l == 0.0 && r == 0.0),
                "tick {via_tick}: stopped, the read plays on"
            );
        }
    }

    /// **A loop moved under a cursor that has been round the old one reads
    /// the file where the cursor is** (the review's B1): wrapped once on
    /// `[1000, 3000)` to frame 1500, then the loop moved to `[2000, 4000)` —
    /// what `VoiceCommand::UpdateLoop` does while a loop point is dragged — the
    /// next frames are the file's 1500, 1501, …, not 3500, …. The same after
    /// the loop is switched off and on again. The ramp's value is its frame + 1.
    ///
    /// Mutation (run): both guards removed — `LoopSpan::taps` wrapping back
    /// every tap behind `resume` when `looped`, and the loop setters leaving
    /// `looped` set → 3501 → fails. (Either guard alone holds it;
    /// `loop_span`'s own test pins the first.)
    #[test]
    fn a_loop_moved_under_a_looped_cursor_reads_where_the_cursor_is() {
        for toggle in [false, true] {
            let mut u = MemorySource::new(ramp_wave(8_000, 44_100.0));
            u.set_loop_range(
                SamplePosition::new(1_000.0),
                SamplePosition::new(3_000.0),
                0,
            );
            u.trigger_at(SamplePosition::new(2_999.0));
            collect_ticks(&mut u, 501);
            assert_eq!(u.position().get(), 1_500.0, "wrapped once, to 1500");
            if toggle {
                u.set_looping(false);
            }
            u.set_loop_range(
                SamplePosition::new(2_000.0),
                SamplePosition::new(4_000.0),
                0,
            );
            let got: Vec<f32> = collect_ticks(&mut u, 3).iter().map(|f| f.0).collect();
            assert_eq!(got, [1_501.0, 1_502.0, 1_503.0], "toggled {toggle}");
        }
    }

    // --- loop wrap arithmetic ---

    #[test]
    fn loop_wrap_handles_overshoot_longer_than_the_loop() {
        // At high varispeed one advance can jump past the loop end by more than
        // the loop's own length. A single `loop_start + (pos - loop_end)`
        // subtraction lands *outside* the region and never recovers; modulo
        // lands inside.
        let wrapped = wrap_into_loop(105.0, 10.0, 20.0);
        assert!(
            (10.0..20.0).contains(&wrapped),
            "overshoot of 8.5 loop lengths must land inside [10, 20), got {wrapped}"
        );
        assert_eq!(wrapped, 15.0);
    }

    #[test]
    fn loop_wrap_is_stable_for_a_single_overshoot() {
        // The common single-overshoot case wraps to the same place a plain
        // subtraction would.
        assert_eq!(wrap_into_loop(22.0, 10.0, 20.0), 12.0);
    }

    #[test]
    fn loop_wrap_survives_a_degenerate_region() {
        // A zero-length region has nothing to wrap into; pin rather than NaN.
        assert_eq!(wrap_into_loop(50.0, 10.0, 10.0), 10.0);
    }

    // --- Existing tests ---

    #[test]
    fn test_sampler_outputs_silence_when_stopped() {
        let wave = Wave::with_capacity(1, 44100.0, 100);
        let mut sampler = MemorySource::new(Arc::new(wave));

        sampler.stop();

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    #[test]
    fn test_loop_crossfade_integration() {
        let samples: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();
        let wave = Wave::from_samples(44100.0, &samples);
        let mut sampler = MemorySource::new(Arc::new(wave));

        sampler.set_loop_range(SamplePosition::new(10.0), SamplePosition::new(90.0), 10);
        sampler.trigger();

        for _ in 0..75 {
            let mut output = [0.0f32; 2];
            sampler.tick(&[], &mut output);
        }

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert!(sampler.is_playing());
    }

    // --- New coverage tests ---

    #[test]
    fn config_default_matches_new() {
        let wave = ramp_wave(100, 44100.0);
        let sampler = MemorySource::with_config(wave, MemorySourceConfig::default());

        // Default config must reproduce `new`'s audible baseline: unity gain,
        // normal speed, one-shot — NOT the newtypes' zero default.
        assert_eq!(sampler.gain(), Amplitude::new(1.0));
        assert_eq!(sampler.speed(), PlaybackRate::new(1.0));
        assert!(!sampler.is_looping());
    }

    #[test]
    fn trigger_at_sets_position() {
        let wave = ramp_wave(100, 44100.0);
        let sampler = MemorySource::new(wave);

        sampler.stop();
        sampler.trigger_at(SamplePosition::new(42.0));
        assert!(sampler.is_playing());
        assert_eq!(sampler.position(), SamplePosition::new(42.0));
    }

    #[test]
    fn reset_clears_position_and_stops() {
        let wave = ramp_wave(100, 44100.0);
        let mut sampler = MemorySource::new(wave);

        // Advance position
        let mut output = [0.0f32; 2];
        for _ in 0..10 {
            sampler.tick(&[], &mut output);
        }
        assert!(sampler.position().get() > 0.0);
        assert!(sampler.is_playing());

        sampler.reset();
        assert_eq!(sampler.position(), SamplePosition::new(0.0));
        assert!(!sampler.is_playing());
    }

    #[test]
    fn mono_wave_duplicates_to_stereo() {
        let wave = ramp_wave(100, 44100.0);
        let mut sampler = MemorySource::new(wave);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], output[1]);
        assert!(output[0] > 0.0);
    }

    #[test]
    fn stereo_wave_preserves_channels() {
        let wave = stereo_ramp_wave(100, 44100.0);
        let mut sampler = MemorySource::new(wave);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert!(output[0] > 0.0);
        assert!(output[1] < 0.0);
        assert!((output[0] + output[1]).abs() < 1e-6);
    }

    #[test]
    fn speed_2x_advances_twice_as_fast() {
        let wave = ramp_wave(100, 44100.0);

        let mut normal = MemorySource::new(Arc::clone(&wave));
        let mut fast = MemorySource::new(wave);
        fast.set_speed(PlaybackRate::new(2.0));

        let mut out = [0.0f32; 2];
        for _ in 0..10 {
            normal.tick(&[], &mut out);
            fast.tick(&[], &mut out);
        }

        let normal_pos = normal.position().get();
        let fast_pos = fast.position().get();
        assert!((fast_pos - normal_pos * 2.0).abs() < 1e-6);
    }

    /// **The placement gate must apply `src_ratio` exactly once** — at the unit,
    /// not just at the kernel.
    ///
    /// The in-memory mirror of `disk_voice`'s
    /// `placement_gate_applies_src_ratio_exactly_once`. The trap is passing the
    /// full `read_rate` against a rate argument (`wave.sample_rate()`) that has
    /// already resolved the beat→sample conversion, which applies `src_ratio`
    /// twice.
    ///
    /// The mismatched rate is the whole test. Every other test on this path uses
    /// a wave at the session rate, where `src_ratio` is `UNITY` and a doubled
    /// factor is exactly 1.0 — an 8.8% position error hides there completely.
    ///
    /// Asserted at the unit rather than only at the kernel because the kernel
    /// cannot see this: it takes the rate as an argument, so a caller passing the
    /// wrong one is invisible there. Re-introducing the bug in `window_rate` fails
    /// only this test.
    #[test]
    fn placement_gate_applies_src_ratio_exactly_once() {
        // A 48 kHz wave in a 44.1 kHz session.
        let wave = ramp_wave(200_000, 48_000.0);
        let transport = MockTransport::rolling(Beat::new(4.0), Bpm::new(120.0));
        let mut sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        sampler.set_sample_rate(SampleRate::new(44_100.0));
        assert!(
            (sampler.src_ratio().get() - (48_000.0 / 44_100.0)).abs() < 1e-4,
            "setup: the session rate must produce a non-unity src_ratio, else \
             this test cannot distinguish one application from two"
        );

        // Beat 4 at 120 BPM is two seconds; two seconds of a 48 kHz file is
        // 96,000 file samples.
        let pos = sampler.window_position().expect("inside the window");
        let expected = 2.0 * 48_000.0;
        let doubled = expected * (48_000.0 / 44_100.0);
        assert!(
            (pos.get() - expected).abs() < 1.0,
            "expected ~{expected} file samples, got {} \
             (a double-applied src_ratio gives ~{doubled})",
            pos.get()
        );
    }

    /// A window is geometry: it does not need a clock to exist, and setting one
    /// before the transport is bound must stick.
    ///
    /// Binding order is not fixed, so a setter guarded on "only if placed" loses
    /// the window silently, and the voice plays from beat 0 for its whole
    /// length.
    #[test]
    fn the_window_can_be_set_before_a_clock_is_bound() {
        let wave = ramp_wave(100, 44_100.0);
        let mut sampler = MemorySource::new(wave);
        assert_eq!(sampler.window(), VoiceWindow::default());

        // No clock yet — a placement-guarded setter would drop this silently.
        sampler.set_window(VoiceWindow::span(Beat::new(8.0), BeatDuration::new(4.0)));
        assert_eq!(sampler.start_beat(), Beat::new(8.0));
        assert_eq!(sampler.duration_beats(), Some(BeatDuration::new(4.0)));

        // Binding a clock afterwards must not disturb the window...
        sampler.replace_transport(MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0)));
        assert_eq!(sampler.start_beat(), Beat::new(8.0));
        assert_eq!(sampler.duration_beats(), Some(BeatDuration::new(4.0)));

        // ...and the gate now honours it: beat 0 is before the beat-8 start.
        assert!(sampler.window_position().is_none());
    }

    /// The block-stepping rate must be the SAME rate the gate used, or a block
    /// starts at the right sample and walks away from it — a slow detune rather
    /// than an obvious break.
    #[test]
    fn window_rate_matches_the_gate_and_excludes_src_ratio() {
        let wave = ramp_wave(200_000, 48_000.0);
        let mut sampler = MemorySource::new(wave);
        sampler.set_speed(PlaybackRate::new(2.0));
        sampler.set_sample_rate(SampleRate::new(44_100.0));

        // Varispeed alone: the gate already resolved the file rate.
        assert!((sampler.window_rate().get() - 2.0).abs() < 1e-6);

        // The free-running rate DOES carry the conversion — different question,
        // different quantity, and the two must not be conflated.
        let free = 2.0 * (48_000.0 / 44_100.0);
        assert!(
            (sampler.read_rate().get() - free).abs() < 1e-4,
            "read_rate should still be varispeed x conversion, got {}",
            sampler.read_rate().get()
        );
    }

    /// The read position advances by `wave_rate / session_rate` per tick, so a
    /// file recorded at a different rate than the session plays at the right
    /// pitch instead of the right speed.
    #[test]
    fn src_ratio_tracks_the_rate_mismatch() {
        // (wave rate, session rate, frames advanced per tick)
        for (wave_hz, session_hz, want) in [
            (44100.0f64, 44100.0f64, 1.0f64), // matched: unity
            (48000.0, 24000.0, 2.0),          // file faster: read two frames a tick
            (24000.0, 48000.0, 0.5),          // file slower: read half a frame
        ] {
            let wave = ramp_wave(100, wave_hz);
            let mut sampler = MemorySource::new(wave);
            sampler.set_session_sample_rate(session_hz);

            let mut out = [0.0f32; 2];
            sampler.tick(&[], &mut out);

            let pos = sampler.position().get();
            assert!(
                (pos - want).abs() < 1e-6,
                "a {wave_hz} Hz wave in a {session_hz} Hz session should advance \
                 {want} frames per tick, got {pos}"
            );
        }
    }

    #[test]
    fn stops_at_end_when_not_looping() {
        let wave = ramp_wave(10, 44100.0);
        let mut sampler = MemorySource::new(wave);

        let mut out = [0.0f32; 2];
        for _ in 0..20 {
            sampler.tick(&[], &mut out);
        }

        assert!(!sampler.is_playing());
    }

    #[test]
    fn loops_back_when_looping() {
        let wave = ramp_wave(10, 44100.0);
        let mut sampler = MemorySource::new(wave);
        sampler.set_looping(true);

        let mut out = [0.0f32; 2];
        // Tick exactly 10 times → position reaches 10.0, wraps to 0.0
        for _ in 0..10 {
            sampler.tick(&[], &mut out);
        }
        assert!(sampler.is_playing());
        let pos = sampler.position().get();
        assert!(
            (pos - 0.0).abs() < 1e-6,
            "10 ticks at speed=1 on len=10 should wrap to 0.0, got {pos}"
        );

        // One more tick reads sample[0] (pos=0.0 after wrap) = 1.0,
        // then advances position to 1.0.
        sampler.tick(&[], &mut out);
        assert!(
            (out[0] - 1.0).abs() < 1e-6,
            "after wrap to 0.0, should read sample[0] = 1.0, got {}",
            out[0]
        );
        let pos_after = sampler.position().get();
        assert!(
            (pos_after - 1.0).abs() < 1e-6,
            "position should advance to 1.0, got {pos_after}"
        );
    }

    #[test]
    fn looping_overshoot_at_double_speed() {
        let wave = ramp_wave(10, 44100.0);
        let mut sampler = MemorySource::new(wave);
        sampler.set_looping(true);
        sampler.set_speed(PlaybackRate::new(2.0));

        let mut out = [0.0f32; 2];
        // 5 ticks at speed=2 → position advances 0,2,4,6,8 → after tick 5
        // position = 10.0, wraps to 0.0
        for _ in 0..5 {
            sampler.tick(&[], &mut out);
        }
        assert!(sampler.is_playing());
        let pos = sampler.position().get();
        assert!(
            (pos - 0.0).abs() < 1e-6,
            "5 ticks at speed=2 on len=10 should wrap to 0.0, got {pos}"
        );

        // 6th tick reads sample[0] = 1.0, advances to 2.0
        sampler.tick(&[], &mut out);
        assert!(
            (out[0] - 1.0).abs() < 1e-6,
            "after wrap, sample[0] should be 1.0, got {}",
            out[0]
        );
    }

    #[test]
    fn process_block_produces_correct_samples() {
        let wave = ramp_wave(100, 44100.0);
        let mut sampler = MemorySource::new(wave);

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        sampler.process(4, &input, &mut output);

        assert!((output.at_f32(0, 0) - 1.0).abs() < 1e-6);
        assert!((output.at_f32(0, 1) - 2.0).abs() < 1e-6);
        assert!((output.at_f32(0, 2) - 3.0).abs() < 1e-6);
        assert!((output.at_f32(0, 3) - 4.0).abs() < 1e-6);
    }

    #[test]
    fn process_block_silence_when_stopped() {
        let wave = ramp_wave(100, 44100.0);
        let mut sampler = MemorySource::new(wave);
        sampler.stop();

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        sampler.process(4, &input, &mut output);

        for i in 0..4 {
            assert_eq!(output.at_f32(0, i), 0.0);
            assert_eq!(output.at_f32(1, i), 0.0);
        }
    }

    #[test]
    fn process_stops_mid_block_when_sample_ends() {
        let wave = ramp_wave(3, 44100.0);
        let mut sampler = MemorySource::new(wave);

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);

        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        sampler.process(8, &input, &mut output);

        assert!(!sampler.is_playing());
        assert!((output.at_f32(0, 0) - 1.0).abs() < 1e-6);
        assert!((output.at_f32(0, 1) - 2.0).abs() < 1e-6);
        assert!((output.at_f32(0, 2) - 3.0).abs() < 1e-6);
        assert_eq!(output.at_f32(0, 4), 0.0);
    }

    // --- Transport-driven playback ---

    #[test]
    fn transport_driven_produces_samples_at_beat_position() {
        // At 120 BPM, beat 1.0 = 0.5 seconds = 22050 samples at 44100 Hz.
        // ramp_wave has sample[i] = i+1, so sample[22050] = 22051.0.
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
        let mut sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        let expected_sample_idx = 22050.0; // 1 beat * 60/120 * 44100
        let expected_value = expected_sample_idx + 1.0; // ramp offset
        assert!(
            (output[0] - expected_value).abs() < 1.0,
            "beat 1.0 @ 120BPM/44.1k should read near sample 22050, got {}",
            output[0]
        );
    }

    #[test]
    fn transport_stopped_outputs_silence() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0));
        let mut sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    #[test]
    fn transport_before_start_beat_outputs_silence() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
        let mut sampler = MemorySource::with_transport(wave, transport, Beat::new(4.0), None);

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0, "beat 1.0 < start_beat 4.0 → silence");
    }

    #[test]
    fn transport_past_duration_beats_outputs_silence() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::rolling(Beat::new(10.0), Bpm::new(120.0));
        let mut sampler = MemorySource::with_transport(
            wave,
            transport,
            Beat::new(0.0),
            Some(BeatDuration::new(4.0)),
        );

        let mut output = [0.0f32; 2];
        sampler.tick(&[], &mut output);

        assert_eq!(output[0], 0.0, "beat 10.0 past duration 4.0 → silence");
    }

    #[test]
    fn transport_process_block() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let mut sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        sampler.process(4, &input, &mut output);

        assert!(
            (output.at_f32(0, 0) - 1.0).abs() < 1e-6,
            "beat 0 → sample 0"
        );
    }

    #[test]
    fn transport_process_block_silence_when_stopped() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0));
        let mut sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);

        let input_vec = BufferVec::new(0);
        let mut output_vec = BufferVec::new(2);
        let input = input_vec.buffer_ref();
        let mut output = output_vec.buffer_mut();
        sampler.process(4, &input, &mut output);

        for i in 0..4 {
            assert_eq!(output.at_f32(0, i), 0.0);
        }
    }

    // --- Clone ---

    #[test]
    fn clone_preserves_state() {
        let wave = ramp_wave(100, 44100.0);
        let sampler = MemorySource::with_config(
            wave,
            MemorySourceConfig {
                gain: Amplitude::new(0.75),
                speed: PlaybackRate::new(1.5),
                loop_setting: LoopSetting::On {
                    start: SamplePosition::new(0.0),
                    end: SamplePosition::new(100.0),
                    crossfade_frames: 0,
                },
                ..Default::default()
            },
        );
        sampler.trigger_at(SamplePosition::new(42.0));

        let cloned = sampler.clone();
        assert_eq!(cloned.gain(), Amplitude::new(0.75));
        assert_eq!(cloned.speed(), PlaybackRate::new(1.5));
        assert!(cloned.is_looping());
        assert!(cloned.is_playing());
        assert_eq!(cloned.position(), SamplePosition::new(42.0));
    }

    // --- set_wave ---

    #[test]
    fn set_wave_resets_position() {
        let wave1 = ramp_wave(100, 44100.0);
        let wave2 = ramp_wave(50, 48000.0);
        let mut sampler = MemorySource::new(wave1);

        sampler.trigger_at(SamplePosition::new(42.0));
        sampler.set_wave(wave2);

        assert_eq!(sampler.position(), SamplePosition::new(0.0));
        assert_eq!(sampler.duration_samples(), 50);
    }

    // --- set_sample_rate (AudioUnit trait) ---

    #[test]
    fn set_sample_rate_updates_src_ratio() {
        let wave = ramp_wave(100, 48000.0);
        let mut sampler = MemorySource::new(wave);

        sampler.set_sample_rate(SampleRate(24000.0));

        let mut out = [0.0f32; 2];
        sampler.tick(&[], &mut out);
        let pos = sampler.position().get();
        assert!((pos - 2.0).abs() < 1e-6, "48k/24k = 2x advance");
    }

    // --- Interpolation at end of sample ---

    #[test]
    fn interpolation_at_last_sample_clamps() {
        let wave = ramp_wave(3, 44100.0);
        let mut sampler = MemorySource::new(wave);

        let mut out = [0.0f32; 2];
        sampler.tick(&[], &mut out);
        assert!((out[0] - 1.0).abs() < 1e-6);

        sampler.tick(&[], &mut out);
        assert!((out[0] - 2.0).abs() < 1e-6);

        sampler.tick(&[], &mut out);
        assert!((out[0] - 3.0).abs() < 1e-6);
    }

    #[test]
    fn looping_wraps_and_continues_producing_audio() {
        // Verify that looping a 2-sample wave keeps producing the same
        // values cyclically (not silence, not garbage).
        let wave = ramp_wave(2, 44100.0);
        let mut sampler = MemorySource::new(wave);
        sampler.set_looping(true);

        let mut outputs = Vec::new();
        let mut out = [0.0f32; 2];
        for _ in 0..6 {
            sampler.tick(&[], &mut out);
            outputs.push(out[0]);
        }

        assert!(sampler.is_playing());
        // ramp_wave(2) = [1.0, 2.0]. Looping: 1, 2, 1, 2, 1, 2
        assert!((outputs[0] - 1.0).abs() < 1e-6, "cycle 0 sample 0");
        assert!((outputs[2] - 1.0).abs() < 1e-6, "cycle 1 sample 0");
        assert!((outputs[4] - 1.0).abs() < 1e-6, "cycle 2 sample 0");
    }

    /// Channel `c` carries the constant `c + 1`, so a wrong-channel read is a
    /// wrong value rather than a plausible one.
    fn indexed_wave(channels: usize, len: usize) -> Arc<Wave> {
        let mut w = Wave::zero(channels, 44_100.0, len as f64 / 44_100.0);
        for i in 0..w.len() {
            for c in 0..channels {
                w.set(c, i, (c + 1) as f32);
            }
        }
        Arc::new(w)
    }

    /// Declared width, not inferred: a 6-channel wave through `new` still yields
    /// a stereo node. A node that re-arity'd itself from its content would break
    /// `Net` edges already wired against `outputs()`.
    #[test]
    fn new_stays_stereo_even_for_a_wide_wave() {
        let u = MemorySource::new(indexed_wave(6, 32));
        assert_eq!(u.channels(), ChannelLayout::STEREO);
        assert_eq!(u.outputs(), 2);
    }

    #[test]
    fn with_channels_declares_the_width() {
        let u = MemorySource::with_channels(indexed_wave(6, 32), 6usize);
        assert_eq!(u.channels(), ChannelLayout::from(6u16));
        assert_eq!(u.outputs(), 6);
        assert_eq!(
            MemorySource::with_channels(indexed_wave(2, 32), 0usize).channels(),
            ChannelLayout::MONO
        );
    }

    /// `route`'s width must track `outputs()` or fundsp mis-plans this node's
    /// latency — silent except as PDC drift.
    #[test]
    fn route_width_tracks_outputs() {
        for w in [1usize, 2, 6, 8] {
            let mut u = MemorySource::with_channels(indexed_wave(2, 32), w);
            let out = u.route(&SignalFrame::new(0), 44_100.0);
            assert_eq!(
                out.len(),
                u.outputs(),
                "route/outputs disagree at width {w}"
            );
        }
    }

    /// All six channels must reach all six outputs, through BOTH entry points.
    /// `tick` writes the caller's slice directly while `process` scatters from a
    /// stack frame into a planar buffer — different code, so both are checked.
    #[test]
    fn six_channel_wave_reaches_all_six_outputs() {
        let mut u = MemorySource::with_channels(indexed_wave(6, 64), 6usize);

        let mut out = [0.0f32; 6];
        u.tick(&[], &mut out);
        for (c, &got) in out.iter().enumerate() {
            assert!(
                (got - (c + 1) as f32).abs() < 1e-4,
                "tick: channel {c} should carry {}, got {got} ({out:?})",
                c + 1
            );
        }

        let mut u = MemorySource::with_channels(indexed_wave(6, 64), 6usize);
        let input = BufferVec::new(0);
        let mut output = BufferVec::new(6);
        u.process(8, &input.buffer_ref(), &mut output.buffer_mut());
        let buf = output.buffer_ref();
        for c in 0..6 {
            let got = buf.at_f32(c, 0);
            assert!(
                (got - (c + 1) as f32).abs() < 1e-4,
                "process: channel {c} should carry {}, got {got}",
                c + 1
            );
        }
    }

    /// Gain is one scalar across every channel — per-channel level is the mixer
    /// strip's job, not the reader's.
    #[test]
    fn gain_applies_uniformly_across_all_channels() {
        let mut u = MemorySource::with_channels(indexed_wave(6, 64), 6usize);
        u.set_gain(Amplitude::new(0.5));
        let mut out = [0.0f32; 6];
        u.tick(&[], &mut out);
        for (c, &got) in out.iter().enumerate() {
            let want = (c + 1) as f32 * 0.5;
            assert!(
                (got - want).abs() < 1e-4,
                "channel {c}: expected {want}, got {got}"
            );
        }
    }

    /// A looping 6-channel voice must keep every channel through the crossfade.
    /// The crossfade blends in place across the whole frame, so a stereo-shaped
    /// blend would leave channels 2..6 un-faded (or worse, untouched).
    #[test]
    fn six_channel_loop_crossfade_covers_every_channel() {
        let mut u = MemorySource::with_channels(indexed_wave(6, 64), 6usize);
        u.set_loop_range(SamplePosition::new(0.0), SamplePosition::new(16.0), 4);
        let mut out = [0.0f32; 6];
        // Drive past the loop point so the crossfade engages at least once.
        for _ in 0..40 {
            u.tick(&[], &mut out);
            for (c, &s) in out.iter().enumerate() {
                assert!(
                    s.is_finite(),
                    "channel {c} produced a non-finite sample during loop crossfade"
                );
            }
        }
    }

    /// **A gain change must reach a voice that is already rendering.**
    ///
    /// The memory tier's half of the live-value rule (`tutti_nodes`' crate docs
    /// state it). `Net`'s frontend holds clones, so a gain stored **by value**
    /// is written on one copy and rendered from another — a clip's fader stops
    /// having any effect once its voice exists, silently.
    ///
    /// Asserted through a **clone**, which is the only vantage point where the
    /// two storage conventions differ: a by-value field looks perfect until
    /// something clones the unit, and `Net::commit` clones every node on every
    /// graph edit.
    #[test]
    fn a_gain_change_reaches_a_cloned_source() {
        let wave = ramp_wave(64, 44_100.0);
        let unit = MemorySource::new(wave);
        unit.play();

        // The clone stands in for the copy the audio thread renders; the
        // original stands in for the frontend the app writes to.
        let mut rendering = unit.clone();

        unit.set_gain(Amplitude::new(0.25));

        let mut out = [0.0f32; 1];
        rendering.tick(&[], &mut out);

        // The ramp's first frame is 1.0, so the rendered value *is* the gain.
        assert!(
            (out[0] - 0.25).abs() < 1e-4,
            "a gain written on one copy must be seen by the copy that renders; \
             expected ~0.25, got {}. A value near 1.0 means `gain` is still \
             stored by value and the write went nowhere.",
            out[0]
        );
    }

    /// **An isolated source does not share control state with the live one.**
    ///
    /// The constraint that sharing introduces, and the reason
    /// `VoiceSource::isolate`'s `Memory` arm cannot stay a no-op once gain is
    /// shared. The offline render clones the live net and ticks it on a worker
    /// thread **while the original keeps playing**; `AudioUnit::isolate` exists
    /// so a clone can hold shared state safely, by severing it before the
    /// worker touches it.
    ///
    /// Without this, a render would fight live playback: moving a fader during
    /// an export would change the exported audio, or worse, the export's own
    /// setup would change what the user hears.
    #[test]
    fn an_isolated_source_stops_sharing_gain() {
        let wave = ramp_wave(64, 44_100.0);
        let live = MemorySource::new(wave);
        live.play();

        let mut render_copy = live.clone();
        render_copy.isolate();

        // A live fader move after isolation must not reach the render.
        live.set_gain(Amplitude::new(0.1));

        let mut out = [0.0f32; 1];
        render_copy.tick(&[], &mut out);

        assert!(
            (out[0] - 1.0).abs() < 1e-4,
            "an isolated copy must keep the gain it was isolated at, not \
             follow the live one; expected ~1.0, got {}",
            out[0]
        );
    }

    /// The same property through the fork contract's harness, the row every
    /// forkable unit gets (`tutti_graph::contract::IsolateRow`): gain is this
    /// unit's one live cell. `excite` plays each rendered copy, since the
    /// harness's `reset` stops it.
    ///
    /// Mutation: make `isolate_gain` a no-op → "a live move reached the fork".
    #[test]
    fn isolate_snapshots_gain() {
        tutti_graph::contract::IsolateRow::new("MemorySource", || {
            MemorySource::new(ramp_wave(48_000, 48_000.0))
        })
        .excite(|s| s.play())
        .control("gain", |s| s.set_gain(Amplitude::new(0.25)))
        .check();
    }
}
