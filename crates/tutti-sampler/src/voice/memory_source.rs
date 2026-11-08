//! In-memory sample playback with optional loop crossfade.
//!
//! The resident half of the sampler's two playback tiers: a whole `Arc<Wave>` in
//! RAM, indexed at a fractional position, against the disk tier's ring-fed
//! stream. The two share their interpolation kernel and their transport
//! placement gate (`super::interp`) so the same file cannot sound different
//! depending on which tier loaded it.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tutti_codec::Wave;
use tutti_graph::{
    CrossfadeCurve, Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status,
    param_parts,
};
use tutti_types::{
    Amplitude, Beat, BeatDuration, ChannelLayout, Param, Param64, PlaybackRate, ReadRate,
    SamplePosition, SampleRate, Samples, SrcRatio, Tail, UnitParam,
};

use super::clock::{BlockClock, Clock};
use super::interp::{
    Gate, Place, hermite_lanes, place, place_tail, read_looped_frame, split_position, tap,
};
use super::loop_span::{LoopSpan, blend};
use super::slot::Piece;
use super::types::Direction;
use crate::lanes::{Gather, Lane, UNPREPARED_FRAMES, audible, scale};
use crate::{MAX_SAMPLER_CHANNELS, PlaceError, nonempty};

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
    /// Loop over `[start, end)` in file frames, with `crossfade_frames` of loop
    /// crossfade (0 = hard loop, no crossfade).
    ///
    /// The fade actually used. The points are whole frames (truncated),
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
    /// When a change is heard. On the memory tier a change is a store,
    /// heard at the next frame. On a live disk stream (`Command::Loop`) the
    /// butler rewrites the ring it has read ahead, from where the old and new
    /// loops first differ: when that is far enough ahead of the playhead, the
    /// change is heard exactly where the memory tier hears it; when it is at
    /// or near the playhead, it lands about 256 frames past the block being
    /// played (plus up to a butler cycle), crossfaded over the stream's seek
    /// crossfade (`BufferConfig::seek_crossfade_frames`) from what the old
    /// loop would have played. From there on the live voice plays the new
    /// loop exactly as the memory tier does. An export fork taken after the
    /// change reads the new loop from the start of its render.
    On {
        /// First frame of the loop, in file frames.
        start: SamplePosition,
        /// End of the loop (exclusive), in file frames; clamped to the file.
        end: SamplePosition,
        /// Length of the loop crossfade in frames; 0 is a hard loop.
        crossfade_frames: usize,
    },
}

/// The span of timeline a voice occupies: where it starts, and how long it
/// lasts.
///
/// Pure geometry — no clock. A window is a value a voice owns; the
/// clock is the block's (`tutti_graph::Env`), read where the window is
/// gated. So this is `Copy`, and a fork of a voice reads its render's `Env`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VoiceWindow {
    /// Start position in beats on the timeline.
    pub start: Beat,
    /// Duration in beats, or `None` to play the whole source.
    pub duration: Option<BeatDuration>,
}

impl VoiceWindow {
    /// A window starting at `start` and running to the end of the source.
    #[must_use]
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MemorySourceConfig {
    /// Linear output gain, applied as one scalar to every channel.
    /// `Amplitude::new(1.0)` is unity — the `Default` value, and not the
    /// newtype's own zero default, which would ship silent.
    pub gain: Amplitude,
    /// Varispeed. Bounded by [`PlaybackRate`]'s own constructor;
    /// [`PlaybackRate::UNITY`] is normal speed and the `Default`. Composes with
    /// the sample-rate conversion through
    /// [`PlaybackRate::read_rate`](tutti_types::PlaybackRate::read_rate), never
    /// by a bare multiply.
    pub speed: PlaybackRate,
    /// Loop intent. `Off` plays once; `On { .. }` loops over the range and
    /// [`MemorySource::with_config`] primes the crossfade internally.
    pub loop_setting: LoopSetting,
    /// The window of the timeline the source plays, read against the
    /// transport from each block's `Env`. `None` leaves it free-running
    /// (audition / one-shot).
    pub placement: Option<VoiceWindow>,
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
            placement: None,
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
/// A placed voice ([`is_placed`](Self::is_placed)) derives its position
/// from the playhead every frame, read from its block's `Env`, and cannot drift
/// from the transport; a free-running one advances its own cursor by
/// [`read_rate`](Self::read_rate). Which applies decides whether
/// [`position`](Self::position) or [`window_position`](Self::window_position) is
/// the meaningful reading, and which rate the caller must step by.
///
/// # As a graph node
///
/// A graph [`Node`]: no inputs, [`channels`](Self::channels) outputs. It is
/// a [`ParamNode`] whose one param is its gain ([`UnitParam::Volume`]), so it
/// goes in through [`param_parts`]: its controls are that [`ParamSet`], and a
/// fork of it ([`fork_fresh`](ParamNode::fork_fresh)) shares nothing — its own
/// gain cell at the authored value — and plays on its render's `Env`.
///
/// A placed source reads through the block read a pool's slot and a
/// [`VoiceNode`](super::VoiceNode) use, so a transport loop wrap sounds the
/// same on all three: the tail it was playing past the loop's end fades out
/// under the loop's start for 5 ms (equal power for a voice mid-sound, unity
/// for one that begins on the loop's start). `prepare` sizes the tail's
/// lanes; a source driven without a `prepare` reads a frame at a time and
/// cuts at the wrap. A free-running source has no transport to wrap on.
///
/// # Real-time
///
/// Every playback path is allocation-free and lock-free, including the loop
/// change in [`set_loop_range`](Self::set_loop_range) — the audio thread drains
/// `VoiceCommand::UpdateLoop` inline. The one exception is
/// [`set_wave`](Self::set_wave), which is control-thread only.
pub struct MemorySource {
    wave: Arc<Wave>,
    position: Param64<SamplePosition>,

    /// Defaults to true (auto-play).
    playing: AtomicBool,

    /// Linear output gain — shared across clones, unlike every other
    /// control field here: the cell the node's [`ParamSet`] addresses, so a
    /// host's write reaches the node the graph renders. A fork detaches it
    /// ([`fork_fresh`](ParamNode::fork_fresh)).
    gain: Param<Amplitude>,

    /// The block's gain from the node's param port while a control lane
    /// drives it (its last frame), set by [`Node::process`] for that block;
    /// `None` reads [`gain`](Self::gain). Never shared: a clone starts
    /// undriven. Only a standalone node has a port: inside a
    /// [`VoiceNode`](super::VoiceNode) the voice reads its own port and
    /// hands the source its gain, and this stays `None`.
    driven_gain: Option<Amplitude>,

    /// Varispeed — user intent, bounded by the type. Composes with
    /// [`src_ratio`](Self::src_ratio) through
    /// [`PlaybackRate::read_rate`](tutti_types::PlaybackRate::read_rate); never
    /// multiplied by a bare scalar.
    speed: PlaybackRate,

    sample_rate: SampleRate,

    /// Sample-rate conversion — derived from the file and session rates, never
    /// user intent. Distinct from [`speed`](Self::speed) for that reason.
    src_ratio: SrcRatio,

    /// The loop in force. The crossfade is read from the wave in place
    /// (`LoopSpan`), so a change is a store.
    loop_setting: LoopSetting,

    /// The window of the transport the source plays; `None` when it is
    /// free-running on its own cursor.
    placement: Option<VoiceWindow>,

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
    /// Deliberately not derived from `wave.channels()`: a node whose arity
    /// followed its content would re-arity itself in the graph the moment a
    /// wider file was loaded, and edges are wired against its shape.
    /// The wave's own channels are placed into this width by
    /// [`read_frame`](super::interp::read_frame)'s channel policy.
    channels: ChannelLayout,

    /// Where the wave's channels go in a frame of [`channels`](Self::channels):
    /// built from the wave's topology when the source is.
    channel_place: Place,

    /// A block's placed positions when this source is a graph node of its
    /// own, sized from `Prepare::max_block` in `prepare` (empty until then,
    /// and in a clone: a source that only plays in a pool or a `VoiceNode`,
    /// which read through their own scratch, carries none).
    positions: Vec<Option<SamplePosition>>,

    /// The block read's gather ([`read_placed_lanes`](Self::read_placed_lanes)),
    /// sized beside [`positions`](Self::positions) and empty where they are.
    gather: Gather,

    /// The lanes a wrap crossfade's tail is read into, one per output
    /// channel, sized beside [`positions`](Self::positions).
    tail: Vec<Lane>,
}

// Hand-rolled: `wave` is a non-`Debug` `Arc<Wave>`. Print the wave length +
// scalar params; never borrow the `Wave` samples.
impl std::fmt::Debug for MemorySource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySource")
            .field("wave_frames", &self.wave.len())
            .field("position", &self.position.load())
            .field("playing", &self.playing.load(Ordering::Relaxed))
            .field("gain", &self.gain)
            .field("speed", &self.speed)
            .field("sample_rate", &self.sample_rate)
            .field("src_ratio", &self.src_ratio)
            .field("loop_setting", &self.loop_setting)
            .field("placement", &self.placement)
            .finish_non_exhaustive()
    }
}

impl Clone for MemorySource {
    fn clone(&self) -> Self {
        Self {
            wave: Arc::clone(&self.wave),
            position: Param64::new(self.position.load()),
            playing: AtomicBool::new(self.playing.load(Ordering::Relaxed)),
            gain: self.gain.handle(),
            driven_gain: None,
            speed: self.speed,
            sample_rate: self.sample_rate,
            src_ratio: self.src_ratio,
            loop_setting: self.loop_setting,
            placement: self.placement,
            looped: AtomicBool::new(self.looped.load(Ordering::Relaxed)),
            // A copy reads its own blocks' transport.
            clock: Clock::new(),
            channels: self.channels,
            channel_place: self.channel_place.clone(),
            // Unsized, so a clone stays allocation-free: a copy inserted as
            // a node is prepared, which sizes it.
            positions: Vec::new(),
            gather: Gather::empty(),
            tail: Vec::new(),
        }
    }
}

impl MemorySource {
    /// A stereo sampler over `wave`.
    ///
    /// Stays stereo even for a wider wave — see [`channels`](Self::channels).
    /// Use [`with_channels`](Self::with_channels) to declare a different width.
    ///
    /// # Errors
    ///
    /// As [`with_channels`](Self::with_channels).
    pub fn new(wave: Arc<Wave>) -> Result<Self, PlaceError> {
        Self::with_channels(wave, ChannelLayout::STEREO)
    }

    /// A `channels`-wide sampler over `wave`, refusing a wave that cannot be
    /// placed.
    ///
    /// The wave's own channels are placed into this width by
    /// [`read_frame`](super::interp::read_frame)'s channel policy (mono is the
    /// centre or the front pair, anything else by its
    /// [`topology`](Wave::topology)). Control side: it builds the fold from
    /// the wave's topology to the source's width.
    ///
    /// # Errors
    ///
    /// [`PlaceError`] for a wave with a channel no speaker names (a second
    /// LFE, a wide or high speaker), or one the frame has no place for.
    pub fn with_channels(
        wave: Arc<Wave>,
        channels: impl Into<ChannelLayout>,
    ) -> Result<Self, PlaceError> {
        let channels = nonempty(channels.into());
        let channel_place = Place::for_wave(&wave, channels)?;
        let sample_rate = wave.sample_rate();
        Ok(Self {
            wave,
            position: Param64::new(SamplePosition::new(0.0)),
            playing: AtomicBool::new(true),
            gain: Param::new(Amplitude::new(1.0)),
            driven_gain: None,
            speed: PlaybackRate::UNITY,
            sample_rate,
            src_ratio: SrcRatio::UNITY,
            loop_setting: LoopSetting::Off,
            placement: None,
            looped: AtomicBool::new(false),
            clock: Clock::new(),
            channels,
            channel_place,
            positions: Vec::new(),
            gather: Gather::empty(),
            tail: Vec::new(),
        })
    }

    /// Output width — the node's audio outputs.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// Creates a source from an explicit [`MemorySourceConfig`] — the canonical
    /// configurable constructor, matching tutti's `X::new(XConfig)` convention.
    ///
    /// A `LoopSetting::On { crossfade_frames, .. }` loops with that crossfade
    /// (via [`set_loop_range`](Self::set_loop_range)); `crossfade_frames == 0`
    /// loops with no crossfade.
    ///
    /// # Errors
    ///
    /// As [`with_channels`](Self::with_channels).
    pub fn with_config(wave: Arc<Wave>, config: MemorySourceConfig) -> Result<Self, PlaceError> {
        let mut unit = Self {
            gain: Param::new(config.gain),
            speed: config.speed,
            placement: config.placement,
            ..Self::with_channels(wave, config.channels)?
        };
        if let LoopSetting::On {
            start,
            end,
            crossfade_frames,
        } = config.loop_setting
        {
            unit.set_loop_range(start, end, crossfade_frames);
        }
        Ok(unit)
    }

    /// Convenience constructor for the common placed voice: on the
    /// transport at `start_beat` for `duration_beats`, everything else
    /// default.
    ///
    /// Equivalent to `with_config(wave, MemorySourceConfig { placement:
    /// Some(window), ..Default::default() })`, and it exists because it reads better
    /// at the timeline call sites — the same reason tutti-polysynth keeps
    /// convenience constructors alongside its config one. `duration_beats` of
    /// `None` plays the whole source.
    ///
    /// # Errors
    ///
    /// As [`with_channels`](Self::with_channels).
    pub fn placed(
        wave: Arc<Wave>,
        start_beat: Beat,
        duration_beats: Option<BeatDuration>,
    ) -> Result<Self, PlaceError> {
        Self::with_config(
            wave,
            MemorySourceConfig {
                placement: Some(VoiceWindow {
                    start: start_beat,
                    duration: duration_beats,
                }),
                ..Default::default()
            },
        )
    }

    /// Places the source on the transport at `window`, or moves its window.
    /// The next frame reads at the new window.
    ///
    /// A free-running source that is sent this (a free-running voice given
    /// `UpdatePlacement`) becomes transport-gated, as a `DiskVoice` always is.
    pub fn set_window(&mut self, window: VoiceWindow) {
        self.placement = Some(window);
    }

    /// Places this source on the transport at `window`.
    #[must_use]
    pub fn placed_at(mut self, window: VoiceWindow) -> Self {
        self.set_window(window);
        self
    }

    /// Rewinds to sample 0 and starts playing. Two relaxed atomic stores, so this
    /// is safe from the audio thread.
    ///
    /// Only affects the free-running path: a placed voice derives its
    /// position from the transport, so triggering one changes nothing audible.
    pub fn trigger(&self) {
        self.trigger_at(SamplePosition::new(0.0));
    }

    /// [`trigger`](Self::trigger) from an arbitrary [`SamplePosition`] in source
    /// samples rather than from 0.
    pub fn trigger_at(&self, position: SamplePosition) {
        self.position.store(position);
        self.looped.store(false, Ordering::Relaxed);
        self.playing.store(true, Ordering::Relaxed);
    }

    /// Resumes from wherever the cursor sits, without rewinding. Free-running
    /// path only, as with [`trigger`](Self::trigger).
    pub fn play(&self) {
        self.playing.store(true, Ordering::Relaxed);
    }

    /// Stops and holds the cursor where it is. Subsequent frames are silence until
    /// [`play`](Self::play) or [`trigger`](Self::trigger).
    pub fn stop(&self) {
        self.playing.store(false, Ordering::Relaxed);
    }

    /// Whether the free-running cursor is advancing. A placed voice ignores this
    /// flag entirely — its silence comes from the window gate, not from here.
    pub fn is_playing(&self) -> bool {
        self.playing.load(Ordering::Relaxed)
    }

    /// Toggles looping. Enabling loops over the whole sample (unless a range was
    /// already set); disabling drops any range and crossfade.
    pub fn set_looping(&mut self, looping: bool) {
        match (looping, self.loop_setting) {
            // Already looping — keep the existing range/crossfade.
            (true, LoopSetting::On { .. }) => {}
            (true, LoopSetting::Off) => {
                self.loop_setting = LoopSetting::On {
                    start: SamplePosition::new(0.0),
                    end: SamplePosition::new(self.wave.len() as f64),
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
        matches!(self.loop_setting, LoopSetting::On { .. })
    }

    /// The free-running cursor, in fractional source samples from the start of
    /// the wave.
    ///
    /// Meaningful only for an unplaced voice. A placed one derives its position
    /// from the playhead each frame and never writes this — read
    /// [`window_position`](Self::window_position) instead.
    pub fn position(&self) -> SamplePosition {
        self.position.load()
    }

    /// Whether this source plays its window of the transport, read from its
    /// block's `Env` (`false`: free-running on its own cursor).
    pub fn is_placed(&self) -> bool {
        self.placement.is_some()
    }

    /// The window of the timeline this voice plays, or `None` when it is
    /// free-running.
    pub fn window(&self) -> Option<VoiceWindow> {
        self.placement
    }

    /// Length of the loaded wave in frames — one per output sample at unity
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

    /// Publishes a new output gain. A NaN or infinite gain is silence.
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
    /// A fork renders on a worker thread while the original keeps
    /// playing, so a shared control cell would let the two fight: a fader
    /// move during an export would change the exported audio. Sharing the
    /// gain is what gives this tier something to sever on a fork.
    ///
    /// Keeps the *current* value: the render must sound like what it was
    /// forked at, not snap to unity.
    pub(crate) fn detach_gain(&mut self) {
        self.gain.detach();
    }

    /// Sets varispeed. Out-of-range and non-finite values are handled by
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
    /// The step, on both paths. A free-running cursor advances by it once
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
    /// The origin only, never the step. Within the clock's move a read steps
    /// by output frames, and an output frame is `src_ratio` file frames at unit
    /// speed: stepping by this instead reads a 24 kHz file on a 48 kHz clock at
    /// one file frame per output frame, then jumps back at every block (frames
    /// 60…63, then 32, at 64-frame blocks). The two agree only at matched rates,
    /// where `src_ratio` is exactly 1.
    #[inline]
    pub fn window_rate(&self) -> ReadRate {
        self.speed.read_rate(SrcRatio::UNITY)
    }

    /// Replaces the wave data. Resets playback position to the start.
    ///
    /// Call from `graph_mut` — not safe to call from the audio thread directly:
    /// it drops the old wave, and if this source held its last reference that
    /// frees the whole decoded buffer. The audio thread uses
    /// `replace_wave` (crate-private), which hands the old wave back.
    pub fn set_wave(&mut self, wave: Arc<Wave>) {
        drop(self.replace_wave(wave));
    }

    /// [`set_wave`](Self::set_wave) that returns the old wave instead of
    /// dropping it, so it frees nothing and is safe on the audio thread. The
    /// caller owns the returned reference and must release it off the audio
    /// thread (the pool's drain retires it to the control thread).
    #[must_use = "dropping the old wave here frees it on this thread"]
    pub(crate) fn replace_wave(&mut self, wave: Arc<Wave>) -> Arc<Wave> {
        self.sample_rate = wave.sample_rate();
        let old = std::mem::replace(&mut self.wave, wave);
        self.position.store(SamplePosition::new(0.0));
        self.looped.store(false, Ordering::Relaxed);
        old
    }

    /// Re-derives the sample-rate conversion ratio for a new session rate.
    ///
    /// The derivation itself lives in [`SrcRatio::for_rates`] — the butler's
    /// streaming path calls the same function, so the two tiers cannot drift
    /// apart on matched-rate detection or the divide-by-zero guard.
    pub fn set_session_sample_rate(&mut self, session_rate: impl Into<SampleRate>) {
        self.src_ratio = SrcRatio::for_rates(self.wave.sample_rate(), session_rate);
    }

    /// Applies a [`LoopSetting`]: `On` primes the range + crossfade, `Off`
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

    /// Loops over `[loop_start, loop_end)` in source samples, with
    /// `crossfade_frames` frames of loop crossfade (0 = a hard loop).
    ///
    /// The loop plays whole frames (its points are truncated, as the butler
    /// takes a stream's), and the fade leads into the loop's start: the last
    /// `crossfade_frames` before the end blend toward the frames *before* the
    /// start, so the wrap continues seamlessly there — or, with too little
    /// before the start, toward the loop's own head, the wrap then resuming
    /// after it ([`LoopSetting::On`] has the rule). The whole rule is
    /// `LoopSpan`'s, which every tier reads through.
    ///
    /// Allocation-free, and it has to be: `VoiceCommand::UpdateLoop` is
    /// drained by `VoicePool::drain_commands`, which `tick`/`process` call — so
    /// this runs inside the audio callback. It is a store: the fade is read from
    /// the wave in place, never copied.
    pub fn set_loop_range(
        &mut self,
        loop_start: SamplePosition,
        loop_end: SamplePosition,
        crossfade_frames: usize,
    ) {
        self.loop_setting = LoopSetting::On {
            start: loop_start,
            end: loop_end,
            crossfade_frames,
        };
        // The cursor has not been round *this* loop. (The taps would read the
        // right frames either way — they only wrap back for a position on the
        // loop — but the flag belongs to the loop it was set by.)
        self.looped.store(false, Ordering::Relaxed);
    }

    /// Drops the loop range and reverts to one-shot playback. Allocation-free, so
    /// it is safe on the same audio-thread command drain
    /// [`set_loop_range`](Self::set_loop_range) runs on.
    pub fn clear_loop_range(&mut self) {
        self.loop_setting = LoopSetting::Off;
        self.looped.store(false, Ordering::Relaxed);
    }

    /// The loop's `(start, end)` in source samples, or `None` when one-shot.
    /// The end is exclusive. Carries no crossfade length — for that, read
    /// [`loop_setting`](Self::loop_setting).
    pub fn loop_range(&self) -> Option<(SamplePosition, SamplePosition)> {
        match self.loop_setting {
            LoopSetting::On { start, end, .. } => Some((start, end)),
            LoopSetting::Off => None,
        }
    }

    /// The current loop as a public [`LoopSetting`] intent, with the crossfade
    /// length in frames as it was asked for (0 for a hard loop).
    ///
    /// Lets a caller that built this unit imperatively read its loop back as a
    /// value — the `VoicePool` add shim uses it to fold a pre-configured
    /// `MemorySource`'s loop into a `Playback` record.
    pub fn loop_setting(&self) -> LoopSetting {
        self.loop_setting
    }

    /// Reads one un-gained frame into `out` (width = `out.len()`).
    ///
    /// Writes every element, so the caller never pre-zeros. The wave's own
    /// channel count need not match `out.len()` —
    /// [`read_frame`](super::interp::read_frame) owns that policy.
    #[inline]
    pub fn sample_raw_into(&self, position: f64, out: &mut [f32]) {
        let len = self.wave.len() as f64;
        if position >= len {
            out.fill(0.0);
            return;
        }

        // 4-tap cubic Hermite via the shared kernel (idx-1, idx, idx+1, idx+2,
        // bound-clamped), unifying this path with `DiskSource`.
        super::interp::read_frame(&self.wave, &self.channel_place, position, out);
    }

    /// Read one un-gained frame of a placed voice at `pos`, the position
    /// the gate and the step give (see [`placed_positions`](Self::placed_positions)),
    /// into `out`, writing every element.
    ///
    /// - Forward, on a loop: a position at or past the loop's end plays
    ///   inside it, and the loop's fade and seam are read as `LoopSpan` has
    ///   them, as the offline disk reader reads the same stream's loop.
    /// - Reverse mirrors the position about the last frame (`len - 1 -
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
                self.sample_raw_into((len - 1.0 - pos.get()).max(0.0), out);
            }
            Direction::Forward => {
                match LoopSpan::from_setting(self.loop_setting, self.wave.len()) {
                    Some(span) => {
                        let (p, looped) = span.place(pos.get());
                        self.read_looped_raw_into(&span, p, looped, out);
                    }
                    None => self.sample_raw_into(pos.get(), out),
                }
            }
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
        read_looped_frame(&self.wave, &self.channel_place, span, p, looped, out);
    }

    /// The gate a placed read seats at, for a read stepping at
    /// `stretch_rate` as well (a stretcher consuming it): its window, this
    /// wave's rate, and varispeed with the stretch — see
    /// [`stretched_window_position`](Self::stretched_window_position).
    #[inline]
    pub(crate) fn gate(&self, stretch_rate: ReadRate) -> Option<Gate> {
        Some(Gate {
            window: self.placement?,
            source_rate: self.wave.sample_rate(),
            rate: self.window_rate().then(stretch_rate),
        })
    }

    /// The positions a placed read plays over block frames `range`, one per
    /// frame into `out`; `None`s outside its window, on a standing
    /// transport, or when unplaced.
    ///
    /// Gated per frame, seated from the clock, stepped by the read rate
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
    ///
    /// Returns whether any frame has a position (`false`: the read is
    /// silent over the whole range).
    #[inline]
    pub(crate) fn placed_positions(
        &self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        stretch_rate: ReadRate,
        out: &mut [Option<SamplePosition>],
    ) -> bool {
        let Some(gate) = self.gate(stretch_rate) else {
            out.fill(None);
            return false;
        };
        let step = self.read_rate().then(stretch_rate);
        place(clock, range, gate, step, out)
    }

    /// [`placed_positions`](Self::placed_positions) for the tail of a wrap
    /// crossfade (`interp::place_tail`), at unity stretch: the frames of
    /// `range` it sounds over, relative to `range`, or `None`.
    #[inline]
    pub(crate) fn placed_tail(
        &self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        out: &mut [Option<SamplePosition>],
    ) -> Option<Range<usize>> {
        let Some(gate) = self.gate(ReadRate::UNITY) else {
            out.fill(None);
            return None;
        };
        place_tail(
            clock,
            range,
            gate,
            self.read_rate().then(ReadRate::UNITY),
            out,
        )
    }

    /// [`read_placed_into`](Self::read_placed_into) for a block: frame `i`
    /// read at `positions[i]` (`None`: silence) into frame `i` of the first
    /// `n` lanes, every frame written. Un-gained, as `read_placed_into` is.
    ///
    /// A read of a wave that is mono or as wide as the lanes runs the kernel
    /// along time: each frame's four taps are gathered into four lanes
    /// (a scattered read, one per voice position — the gather is the cost
    /// that stays scalar), then the cubic runs over the lanes, where the
    /// compiler vectorises it. Same taps, same fraction, same arithmetic per
    /// frame as [`read_frame`] (unlooped) and `read_looped_frame` (a forward
    /// read on a loop, its taps blended inside the fade), so the same
    /// samples bit for bit; a frame with no sample (no position, or at or
    /// past the end) is read from four zero taps at `t` = 0, which the cubic
    /// returns as exactly `0.0`, as `read_frame`'s callers fill it.
    ///
    /// Where every frame's taps are consecutive frames of the wave, unclamped
    /// (an unlooped read at unity rate, away from the wave's ends), the four
    /// tap lanes *are* the wave, shifted by a frame each: the cubic reads
    /// them in place, and nothing is gathered.
    ///
    /// A wave placed by a fold (another width, or another order) reads a frame
    /// at a time through `read_placed_into`.
    ///
    /// [`read_frame`]: super::interp::read_frame
    pub(crate) fn read_placed_lanes<L: AsMut<[f32]>>(
        &self,
        positions: &[Option<SamplePosition>],
        direction: Direction,
        lanes: &mut [L],
        n: usize,
        gather: &mut Gather,
    ) {
        let frames = positions.len();
        if frames == 0 {
            return;
        }
        let len = self.wave.len();
        let src_ch = self.wave.channels();
        // (A wave past `u32::MAX` frames, 24 hours at 48 kHz, reads a frame
        // at a time: the gather keeps each frame's whole index as a `u32`.)
        let along_time = len > 0
            && len <= u32::MAX as usize
            && (src_ch == 1 || (src_ch == n && self.channel_place.is_direct()));
        if !along_time {
            let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
            for (i, pos) in positions.iter().enumerate() {
                match pos {
                    Some(pos) => self.read_placed_into(*pos, direction, &mut frame[..n]),
                    None => frame[..n].fill(0.0),
                }
                for (lane, &s) in lanes.iter_mut().zip(&frame[..n]) {
                    lane.as_mut()[i] = s;
                }
            }
            return;
        }
        let span = match direction {
            Direction::Forward => LoopSpan::from_setting(self.loop_setting, len),
            Direction::Reverse => None,
        };
        let Gather {
            index,
            frac,
            live,
            y,
        } = gather;
        let (index, frac, live) = (
            &mut index[..frames],
            &mut frac[..frames],
            &mut live[..frames],
        );
        let last = len - 1;
        let flen = len as f64;
        // Each frame's whole index and fraction, as `read_placed_into` →
        // `sample_raw_into` → `read_frame` (or, on a loop,
        // `read_looped_raw_into` → `LoopSpan::taps`) derive them, written for
        // every frame: one with no sample gathers zeros at `t` = 0, which the
        // cubic returns as `0.0`. The taps are derived from the index per
        // channel (`tap`, `LoopSpan::frame_taps`).
        for (i, pos) in positions.iter().enumerate() {
            let at = pos.and_then(|pos| {
                let p = pos.get();
                // The reversed mirror, the loop's placement, and the end:
                // `read_placed_into`.
                let p = match (direction, &span) {
                    (Direction::Reverse, _) if p >= flen => return None,
                    (Direction::Reverse, _) => (flen - 1.0 - p).max(0.0),
                    (Direction::Forward, Some(span)) => span.place(p).0,
                    (Direction::Forward, None) => p,
                };
                if p >= flen { None } else { Some(p) }
            });
            (index[i], frac[i], live[i]) = match at {
                Some(at) => {
                    let (idx, f) = split_position(at);
                    // `idx` ≤ `len` (a fraction rounding up to the next
                    // frame), and `len` fits a `u32` (above).
                    (idx as u32, f, true)
                }
                None => (0, 0.0, false),
            };
        }
        let (index, frac, live) = (&*index, &*frac, &*live);
        // Whether every frame reads consecutive frames of the wave, its
        // taps unclamped (and, on a loop, plain: `LoopSpan::plain`). Its own
        // pass, which a varispeed read leaves at its first frames.
        let run = live.iter().all(|&on| on)
            && index.windows(2).all(|w| w[1] == w[0].wrapping_add(1))
            && span.as_ref().is_none_or(|span| {
                positions.iter().zip(index).all(|(pos, &idx)| {
                    pos.is_some_and(|pos| span.plain(idx as usize, span.wrapped(pos.get())))
                })
            });
        // Consecutive taps, clamped nowhere: frames `first - 1` through
        // `first + frames + 1`, all inside the wave.
        let first = index[0] as usize;
        let in_place = run && first >= 1 && first + frames + 2 <= len;
        // Mono fans one interpolated lane to every channel.
        let reads = if src_ch == 1 { 1 } else { n };
        for (c, lane) in lanes.iter_mut().enumerate().take(reads) {
            let wave = self.wave.channel(c);
            let out = &mut lane.as_mut()[..frames];
            if in_place {
                let from = first - 1;
                hermite_lanes(
                    out,
                    [
                        &wave[from..],
                        &wave[from + 1..],
                        &wave[from + 2..],
                        &wave[from + 3..],
                    ],
                    frac,
                );
                continue;
            }
            match &span {
                None => {
                    for (t, lane) in y.iter_mut().enumerate() {
                        for ((s, &idx), &on) in lane.iter_mut().zip(index).zip(live) {
                            *s = if on {
                                wave[tap(idx as usize, last, t)]
                            } else {
                                0.0
                            };
                        }
                    }
                }
                Some(span) => {
                    // A loop's taps wrap through it and blend inside its
                    // fade, as `read_looped_frame` reads them.
                    let [y0, y1, y2, y3] = &mut *y;
                    for (i, pos) in positions.iter().enumerate() {
                        let (idx, looped) = match pos {
                            Some(pos) if live[i] => (index[i] as usize, span.wrapped(pos.get())),
                            _ => {
                                (y0[i], y1[i], y2[i], y3[i]) = (0.0, 0.0, 0.0, 0.0);
                                continue;
                            }
                        };
                        if span.plain(idx, looped) {
                            (y0[i], y1[i], y2[i], y3[i]) =
                                (wave[idx - 1], wave[idx], wave[idx + 1], wave[idx + 2]);
                            continue;
                        }
                        let taps = span.frame_taps(len, idx, looped);
                        let sample = |t: usize| {
                            let tap = taps[t];
                            let tail = wave[tap.frame];
                            match tap.fade {
                                Some((lead, w)) => blend(tail, wave[lead], w),
                                None => tail,
                            }
                        };
                        (y0[i], y1[i], y2[i], y3[i]) = (sample(0), sample(1), sample(2), sample(3));
                    }
                }
            }
            let [y0, y1, y2, y3] = &*y;
            hermite_lanes(
                out,
                [&y0[..frames], &y1[..frames], &y2[..frames], &y3[..frames]],
                frac,
            );
        }
        if src_ch == 1 {
            // `y[0]` is free now: the mono block is placed from a copy while
            // lane 0 is itself rewritten.
            self.channel_place
                .mono()
                .lanes(lanes, n, 0..frames, &mut y[0]);
        }
    }

    /// [`sample_raw_into`](Self::sample_raw_into) with this unit's gain
    /// applied. One scalar gain across every channel — per-channel level is the
    /// mixer strip's job, not the reader's.
    #[inline]
    pub fn sample_into(&self, position: f64, out: &mut [f32]) {
        self.sample_raw_into(position, out);
        let gain = self.block_gain();
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
        let window = self.placement?;
        super::interp::window_position(
            transport,
            window.start,
            window.duration,
            self.wave.sample_rate(),
            self.window_rate(),
        )
    }

    /// [`window_position`](Self::window_position) for a stretched read, where
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
        let window = self.placement?;
        super::interp::window_position(
            transport,
            window.start,
            window.duration,
            self.wave.sample_rate(),
            self.window_rate().then(stretch_rate),
        )
    }

    /// One free-running output frame into `out`, advancing the cursor:
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

        let pos = self.position.load().get();
        let wave_len = self.wave.len() as f64;
        let span = LoopSpan::from_setting(self.loop_setting, self.wave.len());
        match &span {
            Some(span) => {
                let looped = self.looped.load(Ordering::Relaxed);
                self.read_looped_raw_into(span, pos, looped, out);
                self.apply_gain(out);
            }
            None => self.sample_into(pos, out),
        }

        // One output sample's worth of source material.
        let new_pos = pos + self.read_rate().advance(Samples(1)).get();

        // A range with nothing in it keeps its old meaning: the cursor pins to
        // its start (`wrap_into_loop`), rather than play on as one-shot.
        let (looping, loop_start, loop_end) = match (&span, self.loop_setting) {
            (Some(span), _) => (true, span.resume() as f64, span.end() as f64),
            (None, LoopSetting::On { start, end, .. }) => (true, start.get(), end.get()),
            (None, LoopSetting::Off) => (false, 0.0, wave_len),
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
                self.position.store(SamplePosition::new(wrapped));
                self.looped.store(true, Ordering::Relaxed);
            } else {
                self.playing.store(false, Ordering::Relaxed);
                self.position.store(SamplePosition::new(loop_end));
            }
        } else {
            self.position.store(SamplePosition::new(new_pos));
        }
    }

    /// The placed read of block frames `range` into frames `0..range.len()`
    /// of the first `n` `head` lanes, gained by `gain`: the one read a
    /// pool's slot, a [`VoiceNode`](super::VoiceNode) and a bare source all
    /// make. Every frame of each lane is written, silence as zero, unless
    /// the piece is [`Piece::Silent`].
    ///
    /// Across a transport loop wrap the voice's tail past the loop's end is
    /// read into `tail` (lanes as long as the piece) and blended under the
    /// head (`crossfade_wraps`); a block with no wrap reads and gains the
    /// head alone.
    #[allow(clippy::too_many_arguments)]
    #[inline]
    pub(crate) fn render_placed<H: AsMut<[f32]>, T: AsMut<[f32]>>(
        &self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        direction: Direction,
        gain: f32,
        n: usize,
        positions: &mut [Option<SamplePosition>],
        gather: &mut Gather,
        head: &mut [H],
        tail: &mut [T],
    ) -> Piece {
        let frames = range.len();
        let sounds = self.placed_positions(clock, range.clone(), ReadRate::UNITY, positions);
        let fades = clock.fades();
        // With no position, every frame reads +0.0, gained to ±0.0.
        if !sounds && !fades {
            return Piece::Silent;
        }
        self.read_placed_lanes(positions, direction, head, n, gather);
        for lane in head.iter_mut().take(n) {
            scale(&mut lane.as_mut()[..frames], gain);
        }
        if !fades {
            return Piece::Ready;
        }
        let span = self.placed_tail(clock, range.clone(), positions);
        let first = span.as_ref().map_or(0, |s| s.start);
        let mut views: [&mut [f32]; MAX_SAMPLER_CHANNELS] =
            std::array::from_fn(|_| Default::default());
        for (view, lane) in views.iter_mut().zip(tail.iter_mut()) {
            *view = lane
                .as_mut()
                .get_mut(span.clone().unwrap_or(0..0))
                .unwrap_or_default();
        }
        // Only the frames the tail sounds over are read.
        if let Some(span) = &span {
            self.read_placed_lanes(&positions[span.clone()], direction, &mut views, n, gather);
            for view in views.iter_mut().take(n) {
                scale(view, gain);
            }
        }
        let blended = self.gate(ReadRate::UNITY).is_some_and(|gate| {
            crossfade_wraps(clock, range, gate, first, positions, &views, head, n)
        });
        if !blended && !sounds {
            return Piece::Silent;
        }
        Piece::Ready
    }

    /// Render block frames `range` (at most `positions.len()`)
    /// into frame `i - range.start` of each of `out`'s channels: the node's
    /// own read, gained by its own gain.
    ///
    /// One algorithm per position model. Placed: the positions
    /// [`placed_positions`](Self::placed_positions) gives, each read forward
    /// ([`read_placed_into`](Self::read_placed_into)) and gained, silence
    /// outside the window; a block at a time
    /// ([`read_placed_lanes`](Self::read_placed_lanes)) through `gather`
    /// when it is sized for the range, else a frame at a time (a source
    /// driven by hand, unprepared). Free-running:
    /// [`free_frame_into`](Self::free_frame_into) per frame.
    fn render_range(
        &mut self,
        clock: &BlockClock<'_>,
        range: Range<usize>,
        positions: &mut [Option<SamplePosition>],
        gather: &mut Gather,
        tail: &mut [Lane],
        out: &mut [&mut [f32]],
    ) {
        let n = out.len().min(MAX_SAMPLER_CHANNELS);
        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
        let base = range.start;
        if self.placement.is_some() {
            let frames = range.len();
            let positions = &mut positions[..frames];
            if gather.frames() >= frames {
                let mut lanes: [&mut [f32]; MAX_SAMPLER_CHANNELS] =
                    std::array::from_fn(|_| Default::default());
                for (lane, ch) in lanes.iter_mut().zip(out.iter_mut()) {
                    *lane = &mut ch[range.clone()];
                }
                let lanes = &mut lanes[..n];
                let gain = self.block_gain();
                let piece = self.render_placed(
                    clock,
                    range,
                    Direction::Forward,
                    gain,
                    n,
                    positions,
                    gather,
                    lanes,
                    tail,
                );
                if piece == Piece::Silent {
                    // Every frame is outside the window: silence, ungained.
                    for lane in lanes.iter_mut() {
                        lane.fill(0.0);
                    }
                }
                return;
            }
            // Unsized scratch (driven by hand, unprepared): a frame at a
            // time, with no wrap crossfade.
            if !self.placed_positions(clock, range.clone(), ReadRate::UNITY, positions) {
                for lane in out.iter_mut().take(n) {
                    lane[range.clone()].fill(0.0);
                }
                return;
            }
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

    /// The gain this block renders at: the driven port's, else the cell's.
    /// A NaN or infinite one is silence ([`audible`]).
    #[inline]
    fn block_gain(&self) -> f32 {
        audible(self.driven_gain.unwrap_or_else(|| self.gain.load()).get())
    }

    /// Scale a frame by this unit's gain.
    #[inline]
    fn apply_gain(&self, out: &mut [f32]) {
        let gain = self.block_gain();
        for s in out.iter_mut() {
            *s *= gain;
        }
    }
}

/// Blend a wrap crossfade's tail into the loop's start, for a voice with a
/// tail (`Gate::tail_sounds`; a voice without one is untouched). Returns
/// whether any run was blended. Where the tail has ended inside the fade
/// (`positions` empty), the head finishes its ramp alone.
/// `tail` holds the frames from `first`; `range` is the piece, whose first
/// frame is lane frame 0.
///
/// A head in the middle of a sound is `head * g_in + tail * g_out` at
/// [`CrossfadeCurve`]'s equal-power gains: the two are different stretches
/// of the source, which add in power. A head that is the clip's own start
/// (`Gate::begins_with_loop`) keeps unity, `head + tail * g_out`: its attack
/// is not ramped, and the join steps as the clip's first pass does. Both
/// sounding at full level, correlated, the sum peaks at the two together.
#[allow(clippy::too_many_arguments)]
fn crossfade_wraps<H: AsMut<[f32]>>(
    clock: &BlockClock<'_>,
    range: Range<usize>,
    gate: Gate,
    first: usize,
    positions: &[Option<SamplePosition>],
    tail: &[&mut [f32]],
    head: &mut [H],
    n: usize,
) -> bool {
    let base = range.start;
    let mut blended = false;
    for run in clock.runs_in(range) {
        let Some(wrap) = run.wrap else { continue };
        if !gate.tail_sounds(&run, &wrap) {
            continue;
        }
        blended = true;
        let own_start = gate.begins_with_loop(&run, &wrap);
        let end = run.end.min(run.start + wrap.left());
        for at in run.start..end {
            let i = at - base;
            let (g_in, g_out) =
                CrossfadeCurve::EqualPower.gains(wrap.ago + at - run.start, wrap.len);
            let g_in = if own_start { 1.0 } else { g_in };
            // A tail that has ended leaves the head to finish its ramp.
            let sounding = positions[i].is_some();
            for (h, t) in head.iter_mut().zip(tail).take(n) {
                let tail = if sounding { t[i - first] * g_out } else { 0.0 };
                let h = &mut h.as_mut()[i];
                *h = *h * g_in + tail;
            }
        }
    }
    blended
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

    /// A block longer than its positions, through a stack array in pieces
    /// of [`UNPREPARED_FRAMES`]: only a source driven by hand without a
    /// `prepare` (no positions) or past the `max_block` it was prepared for
    /// reaches this, so the prepared render never initialises the array.
    #[cold]
    #[inline(never)]
    fn render_unprepared(&mut self, clock: &BlockClock<'_>, frames: usize, out: &mut [&mut [f32]]) {
        let mut positions = [None; UNPREPARED_FRAMES];
        // Unsized: the frame-at-a-time read.
        let mut gather = Gather::empty();
        let mut from = 0;
        while from < frames {
            let to = (from + UNPREPARED_FRAMES).min(frames);
            self.render_range(clock, from..to, &mut positions, &mut gather, &mut [], out);
            from = to;
        }
    }

    /// The longest block its own positions render in one piece (0: never
    /// prepared).
    #[cfg(test)]
    pub(crate) fn scratch_frames(&self) -> usize {
        self.positions.len().min(self.gather.frames())
    }

    /// Rewind the free-running cursor to the start and stop it; forget the
    /// transport. What [`Node::reset`] does.
    fn rewind(&mut self) {
        self.position.store(SamplePosition::new(0.0));
        self.playing.store(false, Ordering::Relaxed);
        self.looped.store(false, Ordering::Relaxed);
        self.clock.reset();
    }
}

impl Node for MemorySource {
    /// No inputs, [`channels`](Self::channels) outputs; a generator, never
    /// skipped.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.channels)
            .with_tail(Tail::Unbounded)
            // A cubic Hermite read per frame and channel, behind the
            // placement gate: measured 16–17 ns per frame and channel,
            // placed or free-running, against `SvfFilterNode`'s 7.2 and
            // `Heavy`'s 256 (doc 013, "The cost model and coarsening").
            .with_cost(tutti_graph::Cost::Moderate)
            // Its gain, as a voice's ([`VOICE_PARAMS`](super::VOICE_PARAMS)):
            // block-rate, read on the block's last frame.
            .with_params(&super::VOICE_PARAMS)
    }

    /// The session rate: the conversion from the wave's rate is derived from
    /// it ([`set_session_sample_rate`](Self::set_session_sample_rate)).
    /// Its block positions are sized from `max_block` (a re-prepare at
    /// another size resizes them).
    fn prepare(&mut self, p: &Prepare) {
        self.set_render_rate(p.sample_rate());
        let frames = p.max_block().get();
        self.positions.clear();
        self.positions.resize(frames, None);
        self.positions.shrink_to_fit();
        self.gather.resize(frames);
        let width = (self.channels.count() as usize).min(MAX_SAMPLER_CHANNELS);
        self.tail.resize_with(width, Vec::new);
        for lane in &mut self.tail {
            lane.clear();
            lane.resize(frames, 0.0);
            lane.shrink_to_fit();
        }
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.driven_gain = io
            .param(0)
            .frames()
            .and_then(|v| v.last().copied())
            .map(|v| Amplitude(v.max(0.0)));
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
        // One piece: in a graph, `prepare` sized the positions for every
        // block.
        if frames <= self.positions.len() {
            let mut positions = std::mem::take(&mut self.positions);
            let mut gather = std::mem::replace(&mut self.gather, Gather::empty());
            let mut tail = std::mem::take(&mut self.tail);
            self.render_range(
                &clock,
                0..frames,
                &mut positions,
                &mut gather,
                &mut tail,
                &mut refs[..n],
            );
            self.positions = positions;
            self.gather = gather;
            self.tail = tail;
        } else {
            self.render_unprepared(&clock, frames, &mut refs[..n]);
        }
        Status::Modified
    }

    /// The free-running cursor rewound and stopped, the transport forgotten.
    fn reset(&mut self) {
        self.rewind();
    }

    fn param_base(&self, k: usize) -> Option<f32> {
        (k == 0).then(|| self.gain.load().get())
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
mod tests;
