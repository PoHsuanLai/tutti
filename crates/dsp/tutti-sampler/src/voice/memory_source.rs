//! In-memory sample playback with optional loop crossfade.
//!
//! The resident half of the sampler's two playback tiers: a whole `Arc<Wave>` in
//! RAM, indexed at a fractional position, against the disk tier's ring-fed
//! stream. The two share their interpolation kernel and their transport
//! placement gate (`super::interp`) so the same file cannot sound different
//! depending on which tier loaded it.

use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tutti_core::{
    Amplitude, AtomicSamplePosition, Beat, BeatDuration, ChannelLayout, Param, PlaybackRate,
    ReadRate, SamplePosition, SampleRate, Samples, SrcRatio, Tail, UnitParam,
};
use tutti_graph::{
    param_parts, Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status,
};
use tutti_io::Wave;

use super::clock::{BlockClock, Clock};
use super::interp::{hermite_lanes, place, read_looped_frame, tap_indices, Gate};
use super::loop_span::LoopSpan;
use super::types::Direction;
use crate::lanes::{Gather, Lanes, LANE_FRAMES};
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
/// **Pure geometry — no clock.** A window is a value a voice owns; the
/// clock is the block's (`tutti_graph::Env`), read where the window is
/// gated. So this is `Copy`, and a fork of a voice needs nothing rebound.
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
/// gain, normal speed, one-shot, free-running. It is hand-written (not
/// derived) because the newtypes default to zero — a derived default would ship
/// silent (`gain = 0`) and frozen (`speed = 0`).
#[derive(Clone, Debug)]
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
    /// Whether the source is **placed** on the transport: it plays
    /// [`window`](Self::window) of the timeline, reading the transport from
    /// each block's `Env`. `false` leaves it free-running (audition /
    /// one-shot).
    pub placed: bool,
    /// Span of timeline the voice occupies. Only consulted when `placed` —
    /// a window off the transport has nothing to be a window *of*.
    pub window: VoiceWindow,
    /// Output width. Defaults to stereo — see [`MemorySource::channels`] for why
    /// this is declared rather than taken from the wave.
    pub channels: ChannelLayout,
}

impl Default for MemorySourceConfig {
    fn default() -> Self {
        Self {
            gain: Amplitude::new(1.0),
            speed: PlaybackRate::UNITY,
            loop_setting: LoopSetting::Off,
            placed: false,
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
/// A **placed** voice ([`is_placed`](Self::is_placed)) derives its position
/// from the playhead every frame, read from its block's `Env`, and cannot drift
/// from the transport; a **free-running** one advances its own cursor by
/// [`read_rate`](Self::read_rate). Which applies decides whether
/// [`position`](Self::position) or [`window_position`](Self::window_position) is
/// the meaningful reading, and which rate the caller must step by.
///
/// # As a graph node
///
/// A native [`Node`]: no inputs, [`channels`](Self::channels) outputs. It is
/// a [`ParamNode`] whose one param is its gain ([`UnitParam::Volume`]), so it
/// goes in through [`param_parts`]: its controls are that [`ParamSet`], and a
/// fork of it ([`fork_fresh`](ParamNode::fork_fresh)) shares nothing — its own
/// gain cell at the authored value — and plays on its render's `Env`, with
/// nothing to rebind.
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
    /// control field here: the cell the node's [`ParamSet`] addresses, so a
    /// host's write reaches the node the graph renders. A fork detaches it
    /// ([`fork_fresh`](ParamNode::fork_fresh)).
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

    /// Whether the source plays its window of the transport (placed) or
    /// its own cursor (free-running).
    ///
    /// Separate from `window` — see [`VoiceWindow`]. The window is always
    /// present because "from beat 0, whole source" is a meaningful default.
    placed: bool,

    /// Span of timeline this voice occupies. Meaningless unless `placed`.
    window: VoiceWindow,

    /// Whether the free-running cursor has been round its loop: then the frame
    /// behind the loop's start is the loop's last (`LoopSpan::taps`). Atomic
    /// for the reason `position` is: [`trigger`](Self::trigger) clears it
    /// through `&self`.
    looped: AtomicBool,

    /// The transport as this source reads it when it is a graph node of
    /// its own (a voice in a pool or a `VoiceNode` reads its owner's).
    clock: Clock,

    /// Output width — the node's audio outputs, fixed at construction.
    ///
    /// Deliberately **not** derived from `wave.channels()`: a node whose arity
    /// followed its content would re-arity itself in the graph the moment a
    /// wider file was loaded, and edges are wired against its shape.
    /// The wave's own width is reconciled against this one by
    /// [`read_frame`](super::interp::read_frame)'s channel policy.
    channels: ChannelLayout,
}

// Hand-rolled: `wave` is a non-`Debug` `Arc<Wave>`. Print the wave length +
// scalar params; never borrow the `Wave` samples.
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
            .field("placed", &self.placed)
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
            placed: self.placed,
            window: self.window,
            looped: AtomicBool::new(self.looped.load(Ordering::Relaxed)),
            // A copy reads its own blocks' transport.
            clock: Clock::new(),
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
            placed: false,
            window: VoiceWindow::default(),
            looped: AtomicBool::new(false),
            clock: Clock::new(),
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

    /// Output width — the node's audio outputs.
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
            placed: config.placed,
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

    /// Convenience constructor for the common placed voice: on the
    /// transport at `start_beat` for `duration_beats`, everything else
    /// default.
    ///
    /// Equivalent to `with_config(wave, MemorySourceConfig { placed: true,
    /// window, ..Default::default() })`, and it exists because it reads better
    /// at the timeline call sites — the same reason tutti-polysynth keeps
    /// convenience constructors alongside its config one. `duration_beats` of
    /// `None` plays the whole source.
    pub fn placed(wave: Arc<Wave>, start_beat: Beat, duration_beats: Option<BeatDuration>) -> Self {
        Self::with_config(
            wave,
            MemorySourceConfig {
                placed: true,
                window: VoiceWindow {
                    start: start_beat,
                    duration: duration_beats,
                },
                ..Default::default()
            },
        )
    }

    /// Move the window. Independent of whether the source is placed — a
    /// window is just geometry, so there is no "only if placed" branch to get
    /// wrong. The next frame reads at the new window.
    pub fn set_window(&mut self, window: VoiceWindow) {
        self.window = window;
    }

    /// Place this source on the transport at `window` (see
    /// [`is_placed`](Self::is_placed)).
    #[must_use]
    pub fn placed_at(mut self, window: VoiceWindow) -> Self {
        self.placed = true;
        self.window = window;
        self
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

    /// Whether this source plays its window of the transport, read from its
    /// block's `Env` (`false`: free-running on its own cursor).
    pub fn is_placed(&self) -> bool {
        self.placed
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
    /// A fork renders on a worker thread **while the original keeps
    /// playing**, so a shared control cell would let the two fight: a fader
    /// move during an export would change the exported audio. Sharing the
    /// gain is what gives this tier something to sever on a fork.
    ///
    /// Keeps the *current* value: the render must sound like what it was
    /// forked at, not snap to unity.
    pub(crate) fn detach_gain(&mut self) {
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
    /// gives (see `placed_positions`): an output frame is
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
    /// the gate and the step give (see [`placed_positions`](Self::placed_positions)),
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

    /// The gate a placed read seats at, for a read stepping at
    /// `stretch_rate` as well (a stretcher consuming it): its window, this
    /// wave's rate, and varispeed with the stretch — see
    /// [`stretched_window_position`](Self::stretched_window_position).
    #[inline]
    pub(crate) fn gate(&self, stretch_rate: ReadRate) -> Gate {
        Gate {
            window: self.window,
            source_rate: self.wave.sample_rate(),
            rate: self.window_rate().then(stretch_rate),
        }
    }

    /// The positions a placed read plays over block frames `range`, one per
    /// frame into `out`; `None`s outside its window, on a standing
    /// transport, or when unplaced.
    ///
    /// **Gated per frame, seated from the clock, stepped by the read rate**
    /// (`interp::place`): the read enters and leaves its window on the
    /// frames the transport reaches its edges, and in between seats where
    /// the gate puts the playhead and steps by
    /// `read_rate × stretch_rate` per frame. The origin comes from the gate
    /// ([`stretched_window_position`](Self::stretched_window_position):
    /// varispeed and the stretch, measured in this wave's frames), the step
    /// from [`read_rate`](Self::read_rate) (varispeed and the conversion) —
    /// the one model the offline disk reader seats by too, so the tiers read
    /// the same positions, frame for frame. Pass [`ReadRate::UNITY`] when
    /// nothing stretches.
    #[inline]
    pub(crate) fn placed_positions(
        &self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        stretch_rate: ReadRate,
        out: &mut [Option<SamplePosition>],
    ) {
        if !self.placed {
            out.fill(None);
            return;
        }
        let step = self.read_rate().then(stretch_rate);
        place(clock, range, self.gate(stretch_rate), step, out);
    }

    /// [`read_placed_into`](Self::read_placed_into) for a block: frame `i`
    /// read at `positions[i]` (`None`: silence) into frame `i` of the first
    /// `n` lanes, every frame written. Un-gained, as `read_placed_into` is.
    ///
    /// An unlooped read (forward, or reversed) of a wave that is mono or as
    /// wide as the lanes runs the kernel **along time**: each frame's four
    /// taps are gathered into four lanes (a scattered read, one per voice
    /// position — the gather is the cost that stays scalar), then the cubic
    /// runs over the lanes, where the compiler vectorises it. Same taps, same
    /// fraction, same arithmetic per frame as [`read_frame`], so the same
    /// samples bit for bit; a frame with no sample (no position, or at or past
    /// the end) is read from four zero taps at `t` = 0, which the cubic
    /// returns as exactly `0.0`, as `read_frame`'s callers fill it. A loop
    /// (its fade blends taps) or a wave folded to another width reads a frame
    /// at a time through `read_placed_into`.
    ///
    /// [`read_frame`]: super::interp::read_frame
    pub(crate) fn read_placed_lanes(
        &self,
        positions: &[Option<SamplePosition>],
        direction: Direction,
        lanes: &mut Lanes,
        n: usize,
        gather: &mut Gather,
    ) {
        let frames = positions.len();
        let len = self.wave.len();
        let src_ch = self.wave.channels();
        let looped = direction == Direction::Forward && self.loop_mode.span(len).is_some();
        let along_time = !looped && len > 0 && (src_ch == 1 || src_ch == n);
        if !along_time {
            let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
            for (i, pos) in positions.iter().enumerate() {
                match pos {
                    Some(pos) => self.read_placed_into(*pos, direction, &mut frame[..n]),
                    None => frame[..n].fill(0.0),
                }
                lanes.put(i, &frame[..n]);
            }
            return;
        }
        // Each frame's taps and fraction, as `read_placed_into` →
        // `get_sample_raw_into` → `read_frame` derive them, written for every
        // frame: one with no sample reads taps 0 at `t` = 0, weighted out.
        let Gather {
            taps,
            frac,
            live,
            y,
        } = gather;
        let flen = len as f64;
        for (i, pos) in positions.iter().enumerate() {
            let at = pos.and_then(|pos| {
                let p = pos.get();
                // The reversed mirror, and the forward end: `read_placed_into`.
                match direction {
                    Direction::Reverse if p >= flen => None,
                    Direction::Reverse => Some((flen - 1.0 - p).max(0.0)),
                    Direction::Forward if p >= flen => None,
                    Direction::Forward => Some(p),
                }
            });
            (taps[i], frac[i], live[i]) = match at {
                Some(at) => {
                    let (t, f) = tap_indices(len, at);
                    (t, f, true)
                }
                None => ([0; 4], 0.0, false),
            };
        }
        let (taps, frac, live) = (&taps[..frames], &frac[..frames], &live[..frames]);
        // Mono fans one interpolated lane to every channel.
        let reads = if src_ch == 1 { 1 } else { n };
        for c in 0..reads {
            let wave = self.wave.channel(c);
            for (t, lane) in y.iter_mut().enumerate() {
                for ((s, tap), &on) in lane.iter_mut().zip(taps).zip(live) {
                    *s = if on { wave[tap[t]] } else { 0.0 };
                }
            }
            let [y0, y1, y2, y3] = &*y;
            let out = &mut lanes.lanes_mut()[c][..frames];
            hermite_lanes(
                out,
                [&y0[..frames], &y1[..frames], &y2[..frames], &y3[..frames]],
                frac,
            );
        }
        if src_ch == 1 {
            let (first, rest) = lanes.lanes_mut().split_at_mut(1);
            for lane in rest.iter_mut().take(n.saturating_sub(1)) {
                lane[..frames].copy_from_slice(&first[0][..frames]);
            }
        }
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

    /// Where a playhead at `transport` sits in this source's samples, or
    /// `None` when outside the window / unplaced. See [`window_position`](super::interp::window_position).
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
    /// the voice plays. `prepare` seeds `src_ratio` from the live graph,
    /// so that reaches every placed voice whose file rate differs from the
    /// session's, and stays invisible at matched rates where `src_ratio` is
    /// `UNITY` and the extra factor is exactly 1.0.
    ///
    /// The disk tier reaches the same product by the mirror-image split. Two
    /// splits, one product — `both_tier_splits_agree_on_the_same_position` pins
    /// the agreement.
    #[inline]
    pub fn window_position(&self, transport: &tutti_graph::Transport) -> Option<SamplePosition> {
        if !self.placed {
            return None;
        }
        super::interp::window_position(
            transport,
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
    pub fn stretched_window_position(
        &self,
        transport: &tutti_graph::Transport,
        stretch_rate: ReadRate,
    ) -> Option<SamplePosition> {
        if !self.placed {
            return None;
        }
        super::interp::window_position(
            transport,
            self.window.start,
            self.window.duration,
            self.wave.sample_rate(),
            self.window_rate().then(stretch_rate),
        )
    }

    /// One **free-running** output frame into `out`, advancing the cursor:
    /// nothing else owns this voice's time, so it steps its own cursor by
    /// `read_rate`. (A placed voice's frames come from
    /// [`placed_positions`](Self::placed_positions) instead.)
    ///
    /// Writes every element of `out` on every path, so a caller never pre-zeros
    /// and a partial write can never leave a stale channel from the previous
    /// block in a trailing slot.
    #[inline]
    fn free_frame_into(&mut self, out: &mut [f32]) {
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

    /// Render block frames `range` (at most [`LANE_FRAMES`](crate::lanes::LANE_FRAMES))
    /// into frame `i - range.start` of each of `out`'s channels: the node's
    /// own read, gained by its own gain.
    ///
    /// **One algorithm per position model.** Placed: the positions
    /// [`placed_positions`](Self::placed_positions) gives, each read forward
    /// ([`read_placed_into`](Self::read_placed_into)) and gained, silence
    /// outside the window. Free-running: [`free_frame_into`](Self::free_frame_into)
    /// per frame.
    fn render_range(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
        out: &mut [&mut [f32]],
    ) {
        let n = out.len().min(MAX_SAMPLER_CHANNELS);
        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
        let base = range.start;
        if self.placed {
            let positions = &mut positions[..range.len()];
            self.placed_positions(clock, range.clone(), ReadRate::UNITY, positions);
            for (k, pos) in positions.iter().enumerate() {
                match pos {
                    None => frame[..n].fill(0.0),
                    Some(pos) => {
                        self.read_placed_into(*pos, Direction::Forward, &mut frame[..n]);
                        self.apply_gain(&mut frame[..n]);
                    }
                }
                for (c, &s) in frame[..n].iter().enumerate() {
                    out[c][base + k] = s;
                }
            }
            return;
        }
        for i in range {
            self.free_frame_into(&mut frame[..n]);
            for (c, &s) in frame[..n].iter().enumerate() {
                out[c][i] = s;
            }
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

impl MemorySource {
    /// Run at `sample_rate`: the conversion from the wave's rate is derived
    /// from it. What [`Node::prepare`] does; allocation-free.
    pub(crate) fn set_render_rate(&mut self, sample_rate: SampleRate) {
        self.sample_rate = sample_rate;
        self.set_session_sample_rate(sample_rate.get());
    }

    /// Rewind the free-running cursor to the start and stop it; forget the
    /// transport. What [`Node::reset`] does.
    fn rewind(&mut self) {
        self.position
            .store(SamplePosition::new(0.0), Ordering::Relaxed);
        self.playing.store(false, Ordering::Relaxed);
        self.looped.store(false, Ordering::Relaxed);
        self.clock.reset();
    }
}

impl Node for MemorySource {
    /// No inputs, [`channels`](Self::channels) outputs; a generator, never
    /// skipped.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.channels).with_tail(Tail::Unbounded)
    }

    /// The session rate: the conversion from the wave's rate is derived from
    /// it ([`set_session_sample_rate`](Self::set_session_sample_rate)).
    fn prepare(&mut self, p: &Prepare) {
        self.set_render_rate(p.sample_rate());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let clock = self.clock.observe(cx.env);
        let frames = io.frames();
        let (_, mut outs) = io.split();
        let width = outs.len();
        let n = width.min(MAX_SAMPLER_CHANNELS);
        // A node wider than the sampler reads leaves the rest silent.
        for c in n..width {
            outs.get(c).fill(0.0);
        }
        let mut refs: [&mut [f32]; MAX_SAMPLER_CHANNELS] =
            std::array::from_fn(|_| Default::default());
        for (slot, ch) in refs.iter_mut().zip(outs.iter_mut()) {
            *slot = ch;
        }
        let mut positions = [None; LANE_FRAMES];
        let mut from = 0;
        while from < frames {
            let to = (from + LANE_FRAMES).min(frames);
            self.render_range(&clock, from..to, &mut positions, &mut refs[..n]);
            from = to;
        }
        Status::Modified
    }

    /// The free-running cursor rewound and stopped, the transport forgotten,
    /// as `AudioUnit::reset` had it.
    fn reset(&mut self) {
        self.rewind();
    }
}

impl ParamNode for MemorySource {
    /// Its gain, as [`UnitParam::Volume`].
    fn param_set(&self) -> ParamSet {
        ParamSet::builder()
            .param(UnitParam::Volume, self.gain.as_atomic())
            .build()
    }

    /// A copy that shares nothing: its own gain cell (at the value this
    /// one's holds), rewound. The wave is shared read-only.
    fn fork_fresh(&self) -> Self {
        let mut fork = self.clone();
        fork.detach_gain();
        fork.rewind();
        fork
    }
}

impl IntoNode for MemorySource {
    type Controls = ParamSet;

    fn into_parts(self) -> NodeParts<ParamSet> {
        param_parts(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockTransport;
    use tutti_core::Bpm;

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

    /// The rate the tests' blocks run at (the transport's frames).
    const SR: f64 = 44_100.0;

    /// One block of `n` frames of `unit` under `t` (a stopped transport when
    /// `None`: a free-running source reads none), at [`SR`]; the transport
    /// is not moved. Every output channel, planar.
    fn planes(unit: &mut MemorySource, t: Option<&MockTransport>, n: usize) -> Vec<Vec<f32>> {
        let stopped = MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0));
        crate::testing::block(unit, t.unwrap_or(&stopped), SR, n)
    }

    /// [`planes`]' first two channels, as `(left, right)` frames (a mono
    /// node's one channel twice).
    fn block(unit: &mut MemorySource, t: Option<&MockTransport>, n: usize) -> Vec<(f32, f32)> {
        let p = planes(unit, t, n);
        let r = p.len().min(2) - 1;
        (0..n).map(|i| (p[0][i], p[r][i])).collect()
    }

    /// `n` one-frame blocks, the transport **not** moved between them.
    fn frames(unit: &mut MemorySource, t: Option<&MockTransport>, n: usize) -> Vec<(f32, f32)> {
        (0..n).map(|_| block(unit, t, 1)[0]).collect()
    }

    /// One one-frame block into `out` (its channels, as many as `out` holds).
    fn frame_into(unit: &mut MemorySource, t: Option<&MockTransport>, out: &mut [f32]) {
        let p = planes(unit, t, 1);
        for (o, c) in out.iter_mut().zip(p) {
            *o = c[0];
        }
    }

    // --- block-size equivalence ---
    //
    // A node is handed blocks of any length, so N one-frame blocks — the
    // transport moved a frame between each, as a host moves it — must equal
    // one N-frame block sample for sample. (The `AudioUnit` era asked this of
    // `tick` against `process`; the native node has one entry point, and
    // the question is now its block length.)
    //
    // KNOWN LIMIT: these are CONSISTENCY checks, not correctness ones. Both
    // run the same read, so a change moves them together. What catches a
    // wrong placed read is `placed_clip_reads_across_a_block_not_dc` and
    // `placed_clip_block_step_follows_playback_rate`, which assert the shape
    // of the output *within* one block.

    /// `n` one-frame blocks with the playhead moving a frame between each
    /// must equal one `n`-frame block. `transport` is the clock both units
    /// play under; `None` for free-running units.
    ///
    /// Advancing matters: with a frozen playhead every placed frame derives the
    /// same position, both paths emit the same constant, and the assertion holds
    /// no matter what the code does. So does a render that is silent (a
    /// playhead past the wave's end), which is why the frames must vary.
    ///
    /// Mutation (run): `render_range` reading every placed frame at the
    /// piece's first position (`positions[0]`) → the `n`-frame block is DC
    /// → "placed @0.5x" fails. (Until the render had to vary, that case
    /// stood at beat 1 — 22 050 frames into a 4 096-frame wave — and passed
    /// on silence under this mutation.)
    fn assert_blocks_match_frames(
        mut a: MemorySource,
        mut b: MemorySource,
        n: usize,
        transport: Option<&Arc<MockTransport>>,
        case: &str,
    ) {
        let single: Vec<(f32, f32)> = (0..n)
            .map(|_| {
                let f = block(&mut a, transport.map(|t| &**t), 1)[0];
                if let Some(t) = transport {
                    t.advance(1, SR);
                }
                f
            })
            .collect();

        // Rewind so the block sees the same span the frames just walked.
        if let Some(t) = transport {
            t.advance(-(n as i64), SR);
        }
        let whole = block(&mut b, transport.map(|t| &**t), n);

        assert!(
            single.windows(2).any(|w| w[0] != w[1]),
            "{case}: the frames do not vary, so the comparison proves nothing"
        );
        assert_eq!(
            single, whole,
            "{case}: {n} one-frame blocks diverged from one {n}-frame block"
        );
    }

    #[test]
    fn one_block_matches_single_frames_free_running() {
        let wave = ramp_wave(64, 44100.0);
        assert_blocks_match_frames(
            MemorySource::new(Arc::clone(&wave)),
            MemorySource::new(wave),
            16,
            None,
            "free-running",
        );
    }

    #[test]
    fn one_block_matches_single_frames_at_non_unity_speed() {
        let wave = ramp_wave(256, 44100.0);
        let build = || {
            let mut u = MemorySource::new(Arc::clone(&wave));
            u.set_speed(PlaybackRate::new(1.5));
            u
        };
        assert_blocks_match_frames(build(), build(), 32, None, "free-running @1.5x");
    }

    #[test]
    fn one_block_matches_single_frames_when_placed() {
        // The case that was actually broken: a placed voice at non-unity speed.
        let wave = ramp_wave(4096, 44100.0);
        // Beat 0.1: 2 205 frames in, read at 0.5x, inside the wave.
        let transport = MockTransport::rolling(Beat::new(0.1), Bpm::new(120.0));
        let build = || {
            MemorySource::with_config(
                Arc::clone(&wave),
                MemorySourceConfig {
                    speed: PlaybackRate::new(0.5),
                    placed: true,
                    ..Default::default()
                },
            )
        };
        assert_blocks_match_frames(build(), build(), 32, Some(&transport), "placed @0.5x");
    }

    #[test]
    fn one_block_matches_single_frames_across_a_loop_wrap() {
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
        assert_blocks_match_frames(build(), build(), 32, None, "loop wrap");
    }

    // --- varispeed actually reaches a placed voice ---
    //
    // The equivalence tests above cannot catch this class on their own: both
    // lengths run the same read, so any change affects them identically.
    // These pin the *behaviour* instead — that speed reaches the placed path at
    // all. A read deriving position without the rate plays a placed voice at
    // 1x no matter what speed was set, and the equivalence pair stays green.

    fn placed_unit(wave: &Arc<Wave>, rate: f32) -> MemorySource {
        MemorySource::with_config(
            Arc::clone(wave),
            MemorySourceConfig {
                speed: PlaybackRate::new(rate),
                placed: true,
                ..Default::default()
            },
        )
    }

    #[test]
    fn placed_clip_honours_speed_in_one_frame() {
        // A ramp wave encodes position in its amplitude, so the sample value at
        // a fixed playhead names which source frame was read.
        // Beat 0.25 @ 120 BPM / 44.1 kHz = 5512.5 samples in, comfortably
        // inside a 16k wave at both 1x and 0.5x.
        let wave = ramp_wave(16_384, 44100.0);
        let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));

        let mut unity = placed_unit(&wave, 1.0);
        let mut half = placed_unit(&wave, 0.5);

        let a = block(&mut unity, Some(&transport), 1)[0].0;
        let b = block(&mut half, Some(&transport), 1)[0].0;

        assert!(a > 0.0 && b > 0.0, "both should be sounding: {a}, {b}");
        assert!(
            (b - a / 2.0).abs() < 2.0,
            "at 0.5x the voice should be half as far in ({a} -> expected ~{}, got {b})",
            a / 2.0
        );
    }

    #[test]
    fn placed_clip_honours_speed_in_a_block() {
        // The same assertion through a longer block, so the pair documents
        // that the two agree for the right reason.
        let wave = ramp_wave(16_384, 44100.0);
        let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));

        let mut unity = placed_unit(&wave, 1.0);
        let mut half = placed_unit(&wave, 0.5);

        let a = block(&mut unity, Some(&transport), 4)[0].0;
        let b = block(&mut half, Some(&transport), 4)[0].0;

        assert!((b - a / 2.0).abs() < 2.0, "expected ~{}, got {b}", a / 2.0);
    }

    /// A placed voice must read ACROSS a block, not emit one frozen frame.
    ///
    /// A block's `Env` carries the transport at its first frame, so deriving
    /// position from that beat alone gives every sample in the block the same
    /// value. The result is constant DC where the material should be moving.
    /// Caught only by asserting *within* one block: the block-length
    /// equivalence tests cannot see it, because a frozen transport freezes
    /// both paths identically.
    #[test]
    fn placed_clip_reads_across_a_block_not_dc() {
        let wave = ramp_wave(16_384, 44100.0);
        let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
        let mut u = placed_unit(&wave, 1.0);

        let block = block(&mut u, Some(&transport), 8);
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
        let mut u = placed_unit(&wave, 0.5);

        let block = block(&mut u, Some(&transport), 8);
        let step = (block[7].0 - block[0].0) / 7.0;
        assert!(
            (step - 0.5).abs() < 0.01,
            "at 0.5x the in-block step should be ~0.5 samples/frame, got {step}"
        );
    }

    /// **A placed wave at another rate than the clock's steps by the
    /// conversion**, across block boundaries, in 64-frame blocks and in
    /// one-frame ones: a 24 kHz ramp on a 48 kHz clock reads file frame
    /// `n / 2` on output frame `n`, a monotone read half a frame per frame,
    /// with no jump where a block starts.
    ///
    /// Stepping by the gate's rate (`window_rate`, varispeed alone) read one
    /// file frame per output frame within a block and re-seated half a block
    /// back at the next (frames 60…63, then 32).
    ///
    /// Mutation (run): `placed_positions`' step `read_rate` → `window_rate`
    /// → output frame 1 reads file frame 1 → fails (64-frame blocks).
    #[test]
    fn a_placed_wave_at_another_rate_steps_by_the_conversion() {
        let wave = ramp_wave(4_096, 24_000.0);
        for len in [64usize, 1] {
            let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
            let mut u = placed_unit(&wave, 1.0);
            u.set_render_rate(SampleRate::new(48_000.0));
            let out = crate::testing::play(&mut u, &transport, 48_000.0, 512, len);
            // From frame 4: the first frames' taps clamp at the file's start.
            for (n, &l) in out[0].iter().enumerate().skip(4) {
                let want = n as f32 / 2.0 + 1.0;
                assert!(
                    (l - want).abs() < 1e-3,
                    "{len}-frame blocks: output frame {n} read {l}, want file frame {} ({want})",
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
    /// stretch: `gate` without `stretch_rate`) → each block seats at 32 →
    /// fails.
    #[test]
    fn a_stretched_placed_read_steps_by_the_conversion_and_the_stretch() {
        let wave = ramp_wave(4_096, 24_000.0);
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let mut u = placed_unit(&wave, 1.0);
        u.set_render_rate(SampleRate::new(48_000.0));
        let mut clock = Clock::new();
        for block in 0..4 {
            let env = transport.env(64, 48_000.0);
            let bc = clock.observe(&env);
            let mut out = [None; 64];
            u.placed_positions(&bc, 0..64, ReadRate(0.5), &mut out);
            for (i, pos) in out.iter().enumerate() {
                let pos = pos.expect("inside the window");
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
        let ticks = frames(&mut u, None, 3);
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

    /// **A rate change lands on the next block, at the position the new
    /// speed gives**: 16 frames at 1×, then 2× on the next block, and the
    /// read is seated where the gate puts the playhead at 2× (elapsed time at
    /// the new speed — what a varispeed change on a placed voice means) and
    /// steps 2 from there.
    ///
    /// (The `AudioUnit` era also pinned a rate change *between two frames of
    /// one clock reading*, reachable only by `tick`; a block reads its rates
    /// once, so there is no such moment.)
    ///
    /// Mutation (run): `placed_positions` stepping by `read_rate` without
    /// the speed (`SrcRatio` alone) → the second block steps 1 → fails.
    #[test]
    fn a_rate_change_lands_on_the_next_block() {
        let wave = ramp_wave(16_384, 44_100.0);
        let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
        let mut u = placed_unit(&wave, 1.0);
        let first = block(&mut u, Some(&transport), 16);
        transport.advance(16, SR);
        u.set_speed(PlaybackRate::new(2.0));
        let second = block(&mut u, Some(&transport), 16);
        for w in first.windows(2) {
            assert!((w[1].0 - w[0].0 - 1.0).abs() < 1e-3, "1x steps 1");
        }
        for w in second.windows(2) {
            assert!((w[1].0 - w[0].0 - 2.0).abs() < 1e-3, "2x steps 2");
        }
        // Beat 0.25 + 16 frames, at 2x: twice the elapsed frames in.
        let elapsed = 0.25 * 60.0 / 120.0 * SR + 16.0;
        assert!(
            (second[0].0 - (2.0 * elapsed + 1.0) as f32).abs() < 1e-2,
            "seated at the gate at 2x: {}",
            second[0].0
        );
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
            let got: Vec<f32> = frames(&mut u, None, 30 + 2 * (30 - resume))
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

    /// **A stopped clock silences a placed read mid-clip**, in a 64-frame
    /// block and in one-frame ones: the clock stops where it stands (its
    /// beat does not move), and the read must not run on as if it still
    /// rolled.
    ///
    /// Mutation (run): `place` ignoring `run.rolling()` → the read plays on
    /// through the stop → fails.
    #[test]
    fn a_stopped_clock_silences_a_placed_read() {
        let wave = ramp_wave(16_384, 44_100.0);
        for single in [false, true] {
            let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
            let mut u = placed_unit(&wave, 1.0);
            let blk = |u: &mut MemorySource| {
                if single {
                    frames(u, Some(&transport), 64)
                } else {
                    block(u, Some(&transport), 64)
                }
            };
            assert!(
                blk(&mut u).iter().all(|&(l, _)| l > 0.0),
                "single frames {single}: rolling, the clip plays"
            );
            transport.set_rolling(false);
            assert!(
                blk(&mut u).iter().all(|&(l, r)| l == 0.0 && r == 0.0),
                "single frames {single}: stopped, the read plays on"
            );
        }
    }

    /// **A stop inside a block silences the read on its frame** (doc 013
    /// §6): the block's `Env` carries the stop as a change at frame 40, and
    /// the clip plays frames 0..40 and not one more.
    ///
    /// Mutation (run): `place` reading the block's first transport for every
    /// frame (ignoring `Env::changes`) → frames 40.. play on → fails.
    #[test]
    fn a_stop_inside_a_block_silences_the_read_on_its_frame() {
        use tutti_graph::{Offset, TransportChanges};
        let wave = ramp_wave(16_384, 44_100.0);
        let transport = MockTransport::rolling(Beat::new(0.25), Bpm::new(120.0));
        let mut u = placed_unit(&wave, 1.0);
        let mut env = transport.env(64, SR);
        let mut changes = TransportChanges::NONE;
        let stop_beat = Beat::new(0.25 + 40.0 * 2.0 / SR);
        changes
            .push(
                Offset::new(40, Samples(64)).unwrap(),
                tutti_graph::Transport::new(false, Bpm::new(120.0), stop_beat, None),
            )
            .unwrap();
        env.changes = changes;
        let out = tutti_graph::contract::drive_in(&mut u, &env, &[], &[], &[]).audio;
        assert!(
            out[0][..40].iter().all(|&s| s > 0.0),
            "plays up to the stop"
        );
        assert!(
            out[0][40..].iter().all(|&s| s == 0.0),
            "silent from the stop"
        );
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
            frames(&mut u, None, 501);
            assert_eq!(u.position().get(), 1_500.0, "wrapped once, to 1500");
            if toggle {
                u.set_looping(false);
            }
            u.set_loop_range(
                SamplePosition::new(2_000.0),
                SamplePosition::new(4_000.0),
                0,
            );
            let got: Vec<f32> = frames(&mut u, None, 3).iter().map(|f| f.0).collect();
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
        frame_into(&mut sampler, None, &mut output);

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
            frame_into(&mut sampler, None, &mut output);
        }

        let mut output = [0.0f32; 2];
        frame_into(&mut sampler, None, &mut output);

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
            frame_into(&mut sampler, None, &mut output);
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
        frame_into(&mut sampler, None, &mut output);

        assert_eq!(output[0], output[1]);
        assert!(output[0] > 0.0);
    }

    #[test]
    fn stereo_wave_preserves_channels() {
        let wave = stereo_ramp_wave(100, 44100.0);
        let mut sampler = MemorySource::new(wave);

        let mut output = [0.0f32; 2];
        frame_into(&mut sampler, None, &mut output);

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
            frame_into(&mut normal, None, &mut out);
            frame_into(&mut fast, None, &mut out);
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
        let mut sampler = MemorySource::placed(wave, Beat::new(0.0), None);
        sampler.set_render_rate(SampleRate::new(44_100.0));
        assert!(
            (sampler.src_ratio().get() - (48_000.0 / 44_100.0)).abs() < 1e-4,
            "setup: the session rate must produce a non-unity src_ratio, else \
             this test cannot distinguish one application from two"
        );

        // Beat 4 at 120 BPM is two seconds; two seconds of a 48 kHz file is
        // 96,000 file samples.
        let pos = sampler
            .window_position(&transport.transport())
            .expect("inside the window");
        let expected = 2.0 * 48_000.0;
        let doubled = expected * (48_000.0 / 44_100.0);
        assert!(
            (pos.get() - expected).abs() < 1.0,
            "expected ~{expected} file samples, got {} \
             (a double-applied src_ratio gives ~{doubled})",
            pos.get()
        );
    }

    /// A window is geometry: it does not need the source to be placed to
    /// exist, and setting one before the source is placed must stick.
    ///
    /// Order is not fixed, so a setter guarded on "only if placed" loses
    /// the window silently, and the voice plays from beat 0 for its whole
    /// length.
    #[test]
    fn the_window_can_be_set_before_the_source_is_placed() {
        let wave = ramp_wave(100, 44_100.0);
        let mut sampler = MemorySource::new(wave);
        assert_eq!(sampler.window(), VoiceWindow::default());

        // Not placed yet — a placement-guarded setter would drop this silently.
        sampler.set_window(VoiceWindow::span(Beat::new(8.0), BeatDuration::new(4.0)));
        assert_eq!(sampler.start_beat(), Beat::new(8.0));
        assert_eq!(sampler.duration_beats(), Some(BeatDuration::new(4.0)));

        // Placing it afterwards (at its own window) must not disturb it...
        let window = sampler.window();
        let sampler = sampler.placed_at(window);
        assert_eq!(sampler.start_beat(), Beat::new(8.0));
        assert_eq!(sampler.duration_beats(), Some(BeatDuration::new(4.0)));

        // ...and the gate now honours it: beat 0 is before the beat-8 start,
        // beat 9 inside.
        let at = |beat: f64| MockTransport::rolling(Beat::new(beat), Bpm::new(120.0)).transport();
        assert!(sampler.window_position(&at(0.0)).is_none());
        assert!(sampler.window_position(&at(9.0)).is_some());
    }

    /// The block-stepping rate must be the SAME rate the gate used, or a block
    /// starts at the right sample and walks away from it — a slow detune rather
    /// than an obvious break.
    #[test]
    fn window_rate_matches_the_gate_and_excludes_src_ratio() {
        let wave = ramp_wave(200_000, 48_000.0);
        let mut sampler = MemorySource::new(wave);
        sampler.set_speed(PlaybackRate::new(2.0));
        sampler.set_render_rate(SampleRate::new(44_100.0));

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
            frame_into(&mut sampler, None, &mut out);

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
            frame_into(&mut sampler, None, &mut out);
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
            frame_into(&mut sampler, None, &mut out);
        }
        assert!(sampler.is_playing());
        let pos = sampler.position().get();
        assert!(
            (pos - 0.0).abs() < 1e-6,
            "10 ticks at speed=1 on len=10 should wrap to 0.0, got {pos}"
        );

        // One more tick reads sample[0] (pos=0.0 after wrap) = 1.0,
        // then advances position to 1.0.
        frame_into(&mut sampler, None, &mut out);
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
            frame_into(&mut sampler, None, &mut out);
        }
        assert!(sampler.is_playing());
        let pos = sampler.position().get();
        assert!(
            (pos - 0.0).abs() < 1e-6,
            "5 ticks at speed=2 on len=10 should wrap to 0.0, got {pos}"
        );

        // 6th tick reads sample[0] = 1.0, advances to 2.0
        frame_into(&mut sampler, None, &mut out);
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

        let output = planes(&mut sampler, None, 4);

        assert!((output[0][0] - 1.0).abs() < 1e-6);
        assert!((output[0][1] - 2.0).abs() < 1e-6);
        assert!((output[0][2] - 3.0).abs() < 1e-6);
        assert!((output[0][3] - 4.0).abs() < 1e-6);
    }

    #[test]
    fn process_block_silence_when_stopped() {
        let wave = ramp_wave(100, 44100.0);
        let mut sampler = MemorySource::new(wave);
        sampler.stop();

        let output = planes(&mut sampler, None, 4);

        for ch in &output[..2] {
            assert_eq!(ch.len(), 4);
            assert!(ch.iter().all(|&s| s == 0.0));
        }
    }

    #[test]
    fn process_stops_mid_block_when_sample_ends() {
        let wave = ramp_wave(3, 44100.0);
        let mut sampler = MemorySource::new(wave);

        let output = planes(&mut sampler, None, 8);

        assert!(!sampler.is_playing());
        assert!((output[0][0] - 1.0).abs() < 1e-6);
        assert!((output[0][1] - 2.0).abs() < 1e-6);
        assert!((output[0][2] - 3.0).abs() < 1e-6);
        assert_eq!(output[0][4], 0.0);
    }

    // --- Transport-driven playback ---

    #[test]
    fn transport_driven_produces_samples_at_beat_position() {
        // At 120 BPM, beat 1.0 = 0.5 seconds = 22050 samples at 44100 Hz.
        // ramp_wave has sample[i] = i+1, so sample[22050] = 22051.0.
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
        let mut sampler = MemorySource::placed(wave, Beat::new(0.0), None);

        let mut output = [0.0f32; 2];
        frame_into(&mut sampler, Some(&transport), &mut output);

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
        let mut sampler = MemorySource::placed(wave, Beat::new(0.0), None);

        let mut output = [0.0f32; 2];
        frame_into(&mut sampler, Some(&transport), &mut output);

        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    #[test]
    fn transport_before_start_beat_outputs_silence() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
        let mut sampler = MemorySource::placed(wave, Beat::new(4.0), None);

        let mut output = [0.0f32; 2];
        frame_into(&mut sampler, Some(&transport), &mut output);

        assert_eq!(output[0], 0.0, "beat 1.0 < start_beat 4.0 → silence");
    }

    #[test]
    fn transport_past_duration_beats_outputs_silence() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::rolling(Beat::new(10.0), Bpm::new(120.0));
        let mut sampler = MemorySource::placed(wave, Beat::new(0.0), Some(BeatDuration::new(4.0)));

        let mut output = [0.0f32; 2];
        frame_into(&mut sampler, Some(&transport), &mut output);

        assert_eq!(output[0], 0.0, "beat 10.0 past duration 4.0 → silence");
    }

    #[test]
    fn transport_process_block() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let mut sampler = MemorySource::placed(wave, Beat::new(0.0), None);

        let output = planes(&mut sampler, Some(&transport), 4);

        assert!((output[0][0] - 1.0).abs() < 1e-6, "beat 0 → sample 0");
    }

    #[test]
    fn transport_process_block_silence_when_stopped() {
        let wave = ramp_wave(44100, 44100.0);
        let transport = MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0));
        let mut sampler = MemorySource::placed(wave, Beat::new(0.0), None);

        let output = planes(&mut sampler, Some(&transport), 4);

        assert_eq!(output[0].len(), 4);
        assert!(output[0].iter().all(|&s| s == 0.0));
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

    // --- prepare (the node's rate) ---

    #[test]
    fn prepare_updates_src_ratio() {
        let wave = ramp_wave(100, 48000.0);
        let mut sampler = MemorySource::new(wave);

        sampler.prepare(&Prepare::new(SampleRate(24000.0), Samples(64)));

        let mut out = [0.0f32; 2];
        frame_into(&mut sampler, None, &mut out);
        let pos = sampler.position().get();
        assert!((pos - 2.0).abs() < 1e-6, "48k/24k = 2x advance");
    }

    // --- Interpolation at end of sample ---

    #[test]
    fn interpolation_at_last_sample_clamps() {
        let wave = ramp_wave(3, 44100.0);
        let mut sampler = MemorySource::new(wave);

        let mut out = [0.0f32; 2];
        frame_into(&mut sampler, None, &mut out);
        assert!((out[0] - 1.0).abs() < 1e-6);

        frame_into(&mut sampler, None, &mut out);
        assert!((out[0] - 2.0).abs() < 1e-6);

        frame_into(&mut sampler, None, &mut out);
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
            frame_into(&mut sampler, None, &mut out);
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
    /// edges already wired against its shape.
    #[test]
    fn new_stays_stereo_even_for_a_wide_wave() {
        let u = MemorySource::new(indexed_wave(6, 32));
        assert_eq!(u.channels(), ChannelLayout::STEREO);
        assert_eq!(u.shape().audio_out, ChannelLayout::STEREO);
    }

    #[test]
    fn with_channels_declares_the_width() {
        let u = MemorySource::with_channels(indexed_wave(6, 32), 6usize);
        assert_eq!(u.channels(), ChannelLayout::from(6u16));
        assert_eq!(u.shape().audio_out.count(), 6);
        assert_eq!(
            MemorySource::with_channels(indexed_wave(2, 32), 0usize).channels(),
            ChannelLayout::MONO
        );
    }

    /// The node's shape is its declared width, a generator's: no inputs, no
    /// latency, never skipped (its tail is unbounded).
    ///
    /// Mutation (run): `shape` declaring stereo whatever the node was built
    /// at → fails at width 1.
    #[test]
    fn the_shape_is_the_declared_width() {
        for w in [1usize, 2, 6, 8] {
            let u = MemorySource::with_channels(indexed_wave(2, 32), w);
            let shape = u.shape();
            assert_eq!(shape.audio_out.count() as usize, w, "width {w}");
            assert_eq!(shape.audio_in.count(), 0);
            assert_eq!(shape.latency.samples(), Samples(0));
            assert_eq!(shape.tail, Tail::Unbounded);
        }
    }

    /// All six channels must reach all six outputs, free-running and placed
    /// (two reads, each scattering into the node's planar outputs).
    #[test]
    fn six_channel_wave_reaches_all_six_outputs() {
        let mut u = MemorySource::with_channels(indexed_wave(6, 64), 6usize);
        let out = planes(&mut u, None, 8);
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let mut placed = MemorySource::with_channels(indexed_wave(6, 64), 6usize)
            .placed_at(VoiceWindow::default());
        let placed = planes(&mut placed, Some(&transport), 8);
        for (what, out) in [("free-running", out), ("placed", placed)] {
            for (c, ch) in out.iter().enumerate() {
                assert!(
                    (ch[0] - (c + 1) as f32).abs() < 1e-4,
                    "{what}: channel {c} should carry {}, got {}",
                    c + 1,
                    ch[0]
                );
            }
        }
    }

    /// Gain is one scalar across every channel — per-channel level is the mixer
    /// strip's job, not the reader's.
    #[test]
    fn gain_applies_uniformly_across_all_channels() {
        let mut u = MemorySource::with_channels(indexed_wave(6, 64), 6usize);
        u.set_gain(Amplitude::new(0.5));
        let mut out = [0.0f32; 6];
        frame_into(&mut u, None, &mut out);
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
            frame_into(&mut u, None, &mut out);
            for (c, &s) in out.iter().enumerate() {
                assert!(
                    s.is_finite(),
                    "channel {c} produced a non-finite sample during loop crossfade"
                );
            }
        }
    }

    /// **A gain change reaches the node the graph renders.**
    ///
    /// The memory tier's half of the live-value rule: a control a host writes
    /// through the node's controls (its [`ParamSet`], or a clone of the
    /// source sharing its cell) must be seen by the node that renders, which
    /// the executor owns and nothing else can reach. Asserted through the
    /// graph: the node inserted with `param_parts`, its gain set through the
    /// controls `IntoNode` handed back.
    ///
    /// Mutation (run): `param_set` building its `ParamSet` over a detached
    /// copy of the gain (`Param::new(self.gain.load())`) → the render stays
    /// at unity → fails.
    #[test]
    fn a_gain_change_reaches_the_rendering_node() {
        use tutti_graph::Solo;
        let wave = ramp_wave(64, 44_100.0);
        let mut solo = Solo::new(
            MemorySource::with_channels(wave, 1usize),
            Prepare::new(SampleRate(SR), Samples(64)),
        );
        assert!(solo.controls().set(UnitParam::Volume, 0.25));
        let out = solo.render(1);
        // The ramp's first frame is 1.0, so the rendered value *is* the gain.
        assert!(
            (out[0][0] - 0.25).abs() < 1e-4,
            "a gain written through the controls must be seen by the node that \
             renders; expected ~0.25, got {}",
            out[0][0]
        );
    }

    /// **A fork of the node shares nothing with it, and starts from the gain
    /// last set** — the native graph's fork check (`assert_param_fork`,
    /// which replaced the `IsolateRow` row): gain is this node's one cell.
    ///
    /// Mutation (run): `fork_fresh` without `detach_gain` → "a live write
    /// reached the fork" → fails.
    #[test]
    fn the_fork_shares_no_gain() {
        tutti_graph::contract::assert_param_fork(MemorySource::new(ramp_wave(48_000, 48_000.0)));
    }

    /// **A fork plays from the render's transport**, with nothing rebound:
    /// a placed source forked out of a live graph reads its window of the
    /// fork's own blocks' `Env`.
    ///
    /// Mutation (run): `fork_fresh` leaving `placed` false → the fork plays
    /// its free-running cursor (stopped by the reset: silence) → fails.
    #[test]
    fn a_placed_fork_reads_the_renders_transport() {
        let wave = ramp_wave(44_100, 44_100.0);
        let live = MemorySource::placed(wave, Beat::new(1.0), None);
        let mut fork = tutti_graph::ParamFork::new(&live).fork_node();
        // Beat 1 is where the window opens; half a beat later (11 025
        // frames at 120 BPM) the ramp reads 11 026.
        let transport = MockTransport::rolling(Beat::new(1.5), Bpm::new(120.0));
        let out = block(&mut fork, Some(&transport), 2);
        assert!((out[0].0 - 11_026.0).abs() < 1e-3, "{:?}", out[0]);
    }
}
