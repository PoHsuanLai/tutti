//! Transport state shared with the plugin each process block.
//!
//! Superset of the fields VST2, VST3, and CLAP plugins consume. Formats
//! that don't surface a given field leave it at its `Default`.
//!
//! Fields are grouped into focused sub-structs ([`TransportFlags`],
//! [`MusicalTiming`], [`TransportPosition`], [`LoopRegion`], [`BarInfo`])
//! so the top-level type stays readable; callers access via
//! `transport.state.playing`, `transport.timing.tempo`, etc.
//!
//! Musical quantities are carried as their engine types ([`TimeSignature`],
//! [`BarNumber`]) rather than as loose integers, and each format host converts at
//! its own boundary via `From` — the same shape `ChannelLayout` uses for speaker
//! arrangements.

use arrayvec::ArrayVec;
use tutti_graph::MAX_TRANSPORT_CHANGES;
use tutti_types::{BarNumber, Seconds, TimeSignature};
use tutti_ump::System;

/// A transport that takes effect part-way into a chunk: from frame
/// [`offset`](Self::offset) of the chunk on, the transport is
/// [`transport`](Self::transport), until the next change or the chunk's end.
///
/// A chunk's own transport ([`TransportInfo`], sent beside it) is the one at
/// its first frame; a change is never at frame 0. A format with an
/// inter-block transport event (CLAP's `CLAP_EVENT_TRANSPORT`) sends one
/// event per change, at its frame. VST3, VST2 and AU read a chunk's first
/// transport alone: none has such an event.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportChange {
    /// The frame within the chunk, a raw `u32` as the wire's MIDI offset is:
    /// the types stop at the IPC boundary.
    pub offset: u32,
    /// The transport from that frame on.
    pub transport: TransportInfo,
}

/// The transport changes inside one chunk, in time order: at most the graph's
/// own bound per block ([`MAX_TRANSPORT_CHANGES`]).
/// Inline, so a chunk's payload never allocates one; a change that does not fit
/// is the producer's to count.
pub type TransportChanges = ArrayVec<TransportChange, MAX_TRANSPORT_CHANGES>;

/// A transport snapshot passed into a plugin's process call.
///
/// The default is a stopped transport at 120 BPM, 4/4, with a `sample_rate` of
/// `0.0`; the `with_*` builders fill in the rest.
///
/// # Examples
///
/// ```
/// use tutti_plugin_types::{BarNumber, TransportInfo};
///
/// let info = TransportInfo::new()
///     .with_sample_rate(48_000.0)
///     .with_tempo(128.0)
///     .with_playing(true)
///     .with_position_quarters(16.0)
///     .with_bar(16.0, BarNumber::default());
/// assert!(info.state.playing);
/// assert_eq!(info.position.samples, None); // the producer reports it separately
/// ```
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportInfo {
    /// Playing / recording / cycling.
    pub state: TransportFlags,
    /// Tempo and time signature at the playhead.
    pub timing: MusicalTiming,
    /// Playhead position, in whichever coordinate systems the host fills.
    pub position: TransportPosition,
    /// Cycle boundaries. Meaningful only when
    /// [`TransportFlags::cycle_active`] is set.
    pub loop_region: LoopRegion,
    /// Where the current bar starts, and its number.
    pub bar: BarInfo,
    /// Sample rate in Hz (vst3 `ProcessContext::sampleRate`). A raw `f64`
    /// rather than `Hz` because it is handed straight to a C ABI, which is
    /// where the unit types stop — and `Hz` is `f32`-backed, so it could not
    /// carry the ABI's value verbatim anyway.
    pub sample_rate: f64,
    /// The host's monotonic clock at the snapshot's first frame, in
    /// nanoseconds on the OS's monotonic clock (`CLOCK_MONOTONIC`; macOS
    /// `CLOCK_UPTIME_RAW`, Core Audio's host time; Windows QPC), the clock
    /// plugins compare it against (vst3 `systemTime`; vst2 `nanoSeconds`).
    ///
    /// `None` when the producer has no clock that relates to the audio: an
    /// offline render runs faster or slower than real time, and a hand-built
    /// snapshot has none. A raw `i64` because that is the ABI's type, and a
    /// nanosecond count is precision `Seconds` (`f32`) cannot carry.
    pub system_time_ns: Option<i64>,
}

/// Returns whether a transport `f64` may be advertised to a plugin as valid.
///
/// Every plugin API has per-field "this value is filled in" flags, and setting
/// one for a NaN or an infinity is worse than leaving it clear: a plugin that
/// trusts the flag does arithmetic with the value, and NaN propagates straight
/// through its timing math into the audio buffer. A cleared flag makes the
/// plugin fall back to its own defaults, which is always recoverable.
///
/// Every format host uses this one gate. This is the finiteness half only. A field with an additional domain rule
/// (tempo must also be positive) applies that at the call site, since the rule
/// is per-field rather than per-type.
#[must_use]
pub fn is_usable(value: f64) -> bool {
    value.is_finite()
}

/// Playback / record / cycle flags.
///
/// Named `TransportFlags` after the plugin-SDK term for exactly this bundle —
/// CLAP and VST2 both call the field `flags`. Distinct from tutti-transport's
/// `TransportState` *trait* (a live-timeline reader, which no plugin node
/// consumes: each reads its block's transport from the graph); this is the
/// wire snapshot.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportFlags {
    /// `true` while the transport is rolling.
    pub playing: bool,
    /// `true` while the host is capturing. Independent of
    /// [`playing`](Self::playing) — a punch-in armed but not yet rolling is
    /// recording without playing.
    pub recording: bool,
    /// `true` when cycle/loop mode is on, which is what makes
    /// [`LoopRegion`] meaningful.
    pub cycle_active: bool,
}

/// Tempo and time signature at the playhead.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MusicalTiming {
    /// Tempo in BPM at the playhead. A raw `f64` rather than `Bpm` because it
    /// is handed straight to a C ABI, which is where the unit types stop. Must
    /// be finite *and* positive before a format flags it valid — see
    /// [`is_usable`], which covers only the finiteness half.
    pub tempo: f64,
    /// How the tempo moves across the block this snapshot opens, in BPM per
    /// sample, until the next transport event: CLAP's `tempo_inc`. `0.0` for
    /// a constant tempo, a stopped transport, or a block the host cannot
    /// describe by one slope (a tempo point inside it). A raw `f64` rather
    /// than a unit because it is handed straight to a C ABI, which is where
    /// the unit types stop. VST2, VST3 and AU have no such field and read
    /// [`tempo`](Self::tempo) alone.
    pub tempo_inc: f64,
    /// The signature in force at the playhead.
    ///
    /// A [`TimeSignature`] rather than a loose `(i32, i32)` pair: each format
    /// wants a different width (CLAP `u16`, VST2/VST3 `i32`), and casting at
    /// three separate boundaries invites `as u16` on a signed value, where a
    /// negative numerator becomes 65535. The conversions live on the type and
    /// validate on the way through.
    pub signature: TimeSignature,
}

impl Default for MusicalTiming {
    /// 120 BPM, 4/4 — the conventional musical defaults.
    fn default() -> Self {
        Self {
            tempo: 120.0,
            tempo_inc: 0.0,
            signature: TimeSignature::default(),
        }
    }
}

/// The playhead position in several coordinate systems.
///
/// Hosts populate whichever the underlying plugin format understands; each
/// format host reads only the fields it needs.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransportPosition {
    /// Sample-accurate project timeline position (vst2 `samplePos`; vst3
    /// `projectTimeSamples`; AU `outCurrentSampleInTimeLine`). Jumps when the
    /// transport loops or relocates.
    ///
    /// The engine's plugin node keeps it as state, as CLAP's
    /// [`seconds`](Self::seconds) (the same time, in frames, rounded): it
    /// rolls by the frames the transport played, through tempo changes,
    /// holds while it is stopped (a stop, a start where it stopped and a
    /// loop edit continue it, inside one chunk too), and is re-seated from
    /// the playhead's beat at the tempo in force
    /// (`beats * 60 / tempo * sample_rate`) only at a jump: the first block,
    /// a seek (stopped or playing), a loop wrap, or a sample-rate change.
    /// The frames played between two reads come from the engine's frame
    /// count on its timeline segment, exact for one tempo change between
    /// them. The engine has no tempo map, so after a tempo change the two
    /// no longer agree; the rolled count is the time actually played.
    ///
    /// `None` means the producer has no project-time position to report (no
    /// usable tempo, or a hand-built snapshot). It is an `Option` rather than
    /// a plain `i64` because neither the VST2 nor the VST3 ABI has a validity
    /// bit for its sample-position field, so a placeholder `0` would travel
    /// as fact; the `Option` makes each format host decide what to send.
    pub samples: Option<i64>,
    /// Monotonic sample counter that does **not** reset on loop/cycle (vst3
    /// `continousTimeSamples`; clap `steady_time`). Free-running plugins (LFOs,
    /// delays) key their timing off this. `0` means "host has no separate
    /// continuous clock" — consumers fall back to [`samples`](Self::samples),
    /// which is itself optional.
    pub continuous_samples: i64,
    /// Quarter notes from project start (vst2 `ppqPos`; vst3
    /// `projectTimeMusic`).
    pub quarters: f64,
    /// Beats from song start (clap `song_pos_beats`). In 4/4 this
    /// matches `quarters`; in other time signatures the host is
    /// responsible for the conversion.
    pub beats: f64,
    /// Seconds from song start (clap `song_pos_seconds`): the engine's
    /// plugin node fills it as [`samples`](Self::samples) over the rate,
    /// unrounded. A host reads `.get()` where it hands the value to a C ABI.
    pub seconds: Seconds,
}

/// Cycle / loop region boundaries in both coordinate systems.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LoopRegion {
    /// Cycle start in quarter notes (vst2/vst3).
    pub start_quarters: f64,
    /// Cycle end in quarter notes (vst2/vst3).
    pub end_quarters: f64,
    /// Cycle start in beats (clap `loop_start_beats`).
    pub start_beats: f64,
    /// Cycle end in beats (clap `loop_end_beats`).
    pub end_beats: f64,
}

/// Where the current bar starts, and its number.
#[derive(Debug, Clone, Copy, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BarInfo {
    /// Position of the current bar in quarter notes (vst2/vst3
    /// `barPositionMusic`). A raw `f64` rather than `Beat` because it is
    /// handed straight to a C ABI, which is where the unit types stop.
    pub position_quarters: f64,
    /// Position of the current bar in beats (clap `bar_start`).
    pub start_beats: f64,
    /// 1-based bar number (clap `bar_number`).
    pub number: BarNumber,
}

impl Default for TransportInfo {
    /// Returns a stopped transport at 120 BPM, 4/4, with a zero sample rate.
    fn default() -> Self {
        Self {
            state: TransportFlags::default(),
            timing: MusicalTiming::default(),
            position: TransportPosition::default(),
            loop_region: LoopRegion::default(),
            bar: BarInfo::default(),
            sample_rate: 0.0,
            system_time_ns: None,
        }
    }
}

impl TransportInfo {
    /// Creates the default transport: stopped, 120 BPM, 4/4.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets tempo in BPM (builder). Not validated here — see [`is_usable`].
    #[must_use]
    pub fn with_tempo(mut self, tempo: f64) -> Self {
        self.timing.tempo = tempo;
        self
    }

    /// Sets the tempo's slope in BPM per sample (builder): CLAP's `tempo_inc`.
    /// Not validated here — see [`is_usable`].
    #[must_use]
    pub fn with_tempo_inc(mut self, tempo_inc: f64) -> Self {
        self.timing.tempo_inc = tempo_inc;
        self
    }

    /// Sets the playing flag (builder).
    #[must_use]
    pub fn with_playing(mut self, playing: bool) -> Self {
        self.state.playing = playing;
        self
    }

    /// Sets the recording flag (builder). Independent of the playing flag.
    #[must_use]
    pub fn with_recording(mut self, recording: bool) -> Self {
        self.state.recording = recording;
        self
    }

    /// Sets the time signature at the playhead (builder).
    #[must_use]
    pub fn with_time_signature(mut self, signature: TimeSignature) -> Self {
        self.timing.signature = signature;
        self
    }

    /// Sets the render rate in Hz (builder).
    #[must_use]
    pub fn with_sample_rate(mut self, sample_rate: f64) -> Self {
        self.sample_rate = sample_rate;
        self
    }

    /// Sets the CLAP-style position (beats and seconds) (builder).
    #[must_use]
    pub fn with_position_beats(mut self, beats: f64, seconds: Seconds) -> Self {
        self.position.beats = beats;
        self.position.seconds = seconds;
        self
    }

    /// Sets the VST-style musical position in quarter notes (builder).
    ///
    /// Does **not** set a sample position: the producer reports that
    /// separately via [`with_position_samples`](Self::with_position_samples),
    /// and one that has none leaves [`TransportPosition::samples`] `None`.
    #[must_use]
    pub fn with_position_quarters(mut self, quarters: f64) -> Self {
        self.position.quarters = quarters;
        self
    }

    /// Sets the project-time sample position (vst2 `samplePos`; vst3
    /// `projectTimeSamples`), the one that jumps on loop/relocate (builder).
    ///
    /// See [`TransportPosition::samples`] for how the engine derives it.
    #[must_use]
    pub fn with_position_samples(mut self, samples: i64) -> Self {
        self.position.samples = Some(samples);
        self
    }

    /// Sets the monotonic continuous sample counter (vst3
    /// `continousTimeSamples`, clap `steady_time`), the one that does not reset
    /// on loop (builder).
    ///
    /// Left at `0`, consumers fall back to the project-time `samples`.
    #[must_use]
    pub fn with_continuous_samples(mut self, continuous_samples: i64) -> Self {
        self.position.continuous_samples = continuous_samples;
        self
    }

    /// Sets the host's monotonic clock reading, in nanoseconds (builder). See
    /// [`TransportInfo::system_time_ns`].
    #[must_use]
    pub fn with_system_time_ns(mut self, nanoseconds: i64) -> Self {
        self.system_time_ns = Some(nanoseconds);
        self
    }

    /// Frames from the snapshot's first frame to the nearest MIDI clock, at
    /// 24 clocks per quarter note (vst2 `samplesToNextClock`, vst3
    /// `samplesToNextClock`): negative when the nearest clock is behind,
    /// as both SDKs define it ("can be negative (nearest)").
    ///
    /// Derived from the musical position, the tempo and the rate, so it is
    /// the grid the [`quarters`](TransportPosition::quarters) the plugin is
    /// told already implies. `None` unless all three are usable: a finite
    /// position, a finite positive tempo and rate (see [`is_usable`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_plugin_types::TransportInfo;
    ///
    /// // 120 BPM at 48 kHz: a quarter note is 24 000 frames, a clock 1 000.
    /// let t = TransportInfo::new()
    ///     .with_sample_rate(48_000.0)
    ///     .with_tempo(120.0)
    ///     .with_position_quarters(2.0 + 0.25 / 24.0);
    /// assert_eq!(t.samples_to_nearest_clock(), Some(-250));
    /// ```
    pub fn samples_to_nearest_clock(&self) -> Option<i32> {
        let clocks_per_quarter = f64::from(System::TIMING_CLOCKS_PER_QUARTER);
        let (quarters, tempo, rate) = (self.position.quarters, self.timing.tempo, self.sample_rate);
        if !(is_usable(quarters) && is_usable(tempo) && is_usable(rate))
            || tempo <= 0.0
            || rate <= 0.0
        {
            return None;
        }
        let clocks = quarters * clocks_per_quarter;
        let to_nearest = (clocks.round() - clocks) / clocks_per_quarter;
        let frames = (to_nearest * 60.0 / tempo * rate).round();
        // Half a clock at the slowest tempo a plugin is told is far inside
        // `i32`; a pathological tempo saturates rather than wraps.
        Some(frames.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32)
    }

    /// Frames from the snapshot's first frame to the next whole beat, as AU's
    /// `outDeltaSampleOffsetToNextBeat` wants it: `0` when the playhead is on a
    /// beat (within half a frame) and when the transport is stopped, since a
    /// stopped playhead never reaches the next one.
    ///
    /// Derived from [`position.beats`](TransportPosition::beats), the same
    /// axis the beat position is reported on, at the snapshot's tempo and
    /// rate. `None` unless all three are usable: a finite position, a finite
    /// positive tempo and rate (see [`is_usable`]).
    ///
    /// # Examples
    ///
    /// ```
    /// use tutti_plugin_types::TransportInfo;
    ///
    /// // 120 BPM at 48 kHz: a beat is 24 000 frames; a quarter beat is left.
    /// let t = TransportInfo::new()
    ///     .with_sample_rate(48_000.0)
    ///     .with_tempo(120.0)
    ///     .with_playing(true)
    ///     .with_position_beats(2.75, tutti_types::Seconds(0.0));
    /// assert_eq!(t.samples_to_next_beat(), Some(6_000));
    /// ```
    // A bare `u32`, not `Samples`: it is AU's `UInt32` ABI value, as
    // `samples_to_nearest_clock` is the VST ABI's `i32`; the unit types stop at
    // a C ABI.
    pub fn samples_to_next_beat(&self) -> Option<u32> {
        let (beats, tempo, rate) = (self.position.beats, self.timing.tempo, self.sample_rate);
        if !(is_usable(beats) && is_usable(tempo) && is_usable(rate)) || tempo <= 0.0 || rate <= 0.0
        {
            return None;
        }
        if !self.state.playing {
            return Some(0);
        }
        let frames_per_beat = 60.0 / tempo * rate;
        let to_next = (beats.ceil() - beats) * frames_per_beat;
        let frames = to_next.round();
        // Just past a beat, the distance to the next is a whole beat less a
        // sliver: that is "on the beat", not "a beat away".
        if frames >= frames_per_beat.round() {
            return Some(0);
        }
        Some(frames.clamp(0.0, f64::from(u32::MAX)) as u32)
    }

    /// Sets every bar field (builder).
    ///
    /// `position_quarters` and `start_beats` take the same value because the
    /// engine measures both in quarter notes — the distinction exists only for
    /// hosts whose CLAP beat axis is notated beats rather than quarters. VST2
    /// and VST3 read `position_quarters`; CLAP reads `start_beats`.
    #[must_use]
    pub fn with_bar(mut self, start_quarters: f64, number: BarNumber) -> Self {
        self.bar.position_quarters = start_quarters;
        self.bar.start_beats = start_quarters;
        self.bar.number = number;
        self
    }

    /// Sets every loop field, plus the cycle-active flag (builder).
    ///
    /// The `_beats` and `_quarters` pairs take the same value for the same
    /// reason [`with_bar`](Self::with_bar) does. VST2 and VST3 read the
    /// `_quarters` pair; CLAP reads the `_beats` pair.
    #[must_use]
    pub fn with_loop(mut self, active: bool, start_beats: f64, end_beats: f64) -> Self {
        self.state.cycle_active = active;
        self.loop_region.start_beats = start_beats;
        self.loop_region.end_beats = end_beats;
        self.loop_region.start_quarters = start_beats;
        self.loop_region.end_quarters = end_beats;
        self
    }
}

/// Tells a format host whether the playhead moved other than by playing, from
/// one block's snapshot to the next.
///
/// Formats that pull transport (AU's `outTransportStateChanged`) need to say
/// "start, stop, or locate since you last asked" so a plugin's internal
/// sequencer resynchronizes instead of counting on from the old position. The
/// host's snapshots carry no such flag, so it is derived: a play/stop flip, or a
/// beat position that is not where the previous block's playing would have put
/// it (a seek, a loop wrap), or the first block after [`reset`](Self::reset).
///
/// Plain data, no allocation: [`observe`](Self::observe) is called on the audio
/// thread.
#[derive(Debug, Clone, Copy, Default)]
pub struct PlayheadWatch {
    /// Whether any block was observed since construction or [`reset`](Self::reset).
    seen: bool,
    /// The last block's `playing`.
    playing: bool,
    /// Where the previous block's playing leaves the playhead; `None` when it
    /// could not be computed (no usable tempo or rate), in which case only a
    /// play/stop flip is detected.
    expected_beats: Option<f64>,
}

impl PlayheadWatch {
    /// Beats of disagreement below which a position is "where it should be".
    /// Tempo ramps move the playhead by far less than this per block; a seek
    /// that small is inaudible.
    const BEAT_TOLERANCE: f64 = 1e-3;

    /// A watch that has seen nothing: its first block reports a change.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Forgets the previous block: the next [`observe`](Self::observe) reports a
    /// change. Call after anything that breaks continuity outside the
    /// snapshots (a sample-rate change, a reset).
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Records this block's snapshot (`frames` long) and returns whether the
    /// transport started, stopped or jumped since the previous block.
    pub fn observe(&mut self, transport: &TransportInfo, frames: u32) -> bool {
        let playing = transport.state.playing;
        let beats = transport.position.beats;
        let jumped = match self.expected_beats {
            // Agreement is tested and negated, not `>` tested: a NaN position
            // is a jump, not agreement.
            Some(expected) => {
                let agrees = (beats - expected).abs() <= Self::BEAT_TOLERANCE;
                !agrees
            }
            None => false,
        };
        let changed = !self.seen || playing != self.playing || jumped;

        let (tempo, rate) = (transport.timing.tempo, transport.sample_rate);
        self.expected_beats = if !is_usable(beats) {
            None
        } else if !playing {
            Some(beats)
        } else if is_usable(tempo) && is_usable(rate) && tempo > 0.0 && rate > 0.0 {
            Some(beats + f64::from(frames) / rate * tempo / 60.0)
        } else {
            None
        };
        self.seen = true;
        self.playing = playing;
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `with_loop` must fill both pairs, exactly as `with_bar` does. Filling
    /// only `_beats` leaves VST2/VST3 — which read `_quarters` — seeing 0..0
    /// while their cycle-valid bit is set anyway, and only CLAP sees the loop.
    #[test]
    fn with_loop_fills_quarters_as_well_as_beats() {
        let t = TransportInfo::new().with_loop(true, 4.0, 16.0);

        assert!(t.state.cycle_active);
        assert_eq!(t.loop_region.start_beats, 4.0);
        assert_eq!(t.loop_region.end_beats, 16.0);
        assert_eq!(t.loop_region.start_quarters, 4.0);
        assert_eq!(t.loop_region.end_quarters, 16.0);
    }

    /// A snapshot nobody reported a project-time position on carries `None`,
    /// not a `0` that VST2/VST3 would forward as fact; the builder that
    /// reports one sets it, and only that builder.
    #[test]
    fn position_samples_is_absent_until_a_host_reports_one() {
        assert_eq!(TransportInfo::new().position.samples, None);
        assert_eq!(
            TransportInfo::new()
                .with_position_quarters(4.0)
                .position
                .samples,
            None
        );
        assert_eq!(
            TransportInfo::new()
                .with_position_samples(1_234)
                .position
                .samples,
            Some(1_234)
        );
    }

    /// **The MIDI clock offset is the distance to the nearest 24-ppq clock,
    /// in frames, signed.** At 120 BPM and 48 kHz a clock is 1 000 frames;
    /// at 90 BPM and 44.1 kHz it is 1 225. A quarter of a clock past a clock
    /// is behind it (negative), three quarters is ahead of the next.
    #[test]
    fn samples_to_nearest_clock_is_the_signed_distance_on_the_24_ppq_grid() {
        let at = |tempo: f64, rate: f64, quarters: f64| {
            TransportInfo::new()
                .with_tempo(tempo)
                .with_sample_rate(rate)
                .with_position_quarters(quarters)
                .samples_to_nearest_clock()
        };
        assert_eq!(at(120.0, 48_000.0, 2.0), Some(0));
        assert_eq!(at(120.0, 48_000.0, 2.0 + 0.25 / 24.0), Some(-250));
        assert_eq!(at(120.0, 48_000.0, 2.0 + 0.75 / 24.0), Some(250));
        assert_eq!(at(90.0, 44_100.0, 1.0 + 0.4 / 24.0), Some(-490));
        assert_eq!(at(90.0, 44_100.0, 1.0 + 0.6 / 24.0), Some(490));

        // Nothing to derive it from.
        assert_eq!(at(0.0, 48_000.0, 2.0), None);
        assert_eq!(at(-120.0, 48_000.0, 2.0), None);
        assert_eq!(at(120.0, 0.0, 2.0), None);
        assert_eq!(at(120.0, 48_000.0, f64::NAN), None);
        assert_eq!(at(f64::INFINITY, 48_000.0, 2.0), None);
    }

    /// **The next-beat offset is the distance to the next whole beat, in
    /// frames; on a beat and while stopped it is zero.** At 120 BPM and 48 kHz
    /// a beat is 24 000 frames; at 90 BPM and 44.1 kHz it is 29 400.
    #[test]
    fn samples_to_next_beat_is_the_distance_to_the_next_whole_beat() {
        let at = |tempo: f64, rate: f64, beats: f64, playing: bool| {
            TransportInfo::new()
                .with_tempo(tempo)
                .with_sample_rate(rate)
                .with_playing(playing)
                .with_position_beats(beats, Seconds(0.0))
                .samples_to_next_beat()
        };
        assert_eq!(at(120.0, 48_000.0, 2.25, true), Some(18_000));
        assert_eq!(at(120.0, 48_000.0, 2.75, true), Some(6_000));
        assert_eq!(at(90.0, 44_100.0, 1.5, true), Some(14_700));
        // On a beat, and a hair past it: no distance, not a whole beat.
        assert_eq!(at(120.0, 48_000.0, 3.0, true), Some(0));
        assert_eq!(at(120.0, 48_000.0, 3.0 + 1e-9, true), Some(0));
        // Stopped: it never gets there.
        assert_eq!(at(120.0, 48_000.0, 2.25, false), Some(0));

        assert_eq!(at(0.0, 48_000.0, 2.25, true), None);
        assert_eq!(at(120.0, 0.0, 2.25, true), None);
        assert_eq!(at(120.0, 48_000.0, f64::NAN, true), None);
    }

    fn block(playing: bool, beats: f64) -> TransportInfo {
        TransportInfo::new()
            .with_tempo(120.0)
            .with_sample_rate(48_000.0)
            .with_playing(playing)
            .with_position_beats(beats, Seconds(0.0))
    }

    /// **A playhead that advances as playing predicts is not a change; the
    /// first block, a start, a stop, a seek and a loop wrap are.** 512 frames
    /// at 120 BPM / 48 kHz is `512 / 24 000` beats.
    #[test]
    fn a_playhead_watch_reports_starts_stops_and_jumps_only() {
        let step = 512.0 / 24_000.0;
        let mut w = PlayheadWatch::new();
        assert!(w.observe(&block(false, 0.0), 512), "first block");
        assert!(!w.observe(&block(false, 0.0), 512), "stopped and still");
        assert!(w.observe(&block(true, 0.0), 512), "start");
        for i in 1..=20 {
            assert!(
                !w.observe(&block(true, f64::from(i) * step), 512),
                "steady block {i}"
            );
        }
        assert!(w.observe(&block(true, 40.0), 512), "seek forward");
        assert!(!w.observe(&block(true, 40.0 + step), 512), "steady again");
        assert!(w.observe(&block(true, 8.0), 512), "loop wrap back");
        assert!(w.observe(&block(false, 8.0 + step), 512), "stop");
        // Stopped: the playhead is expected where it is.
        assert!(!w.observe(&block(false, 8.0 + step), 512), "stopped, still");
        assert!(w.observe(&block(false, 3.0), 512), "seek while stopped");
    }

    /// `reset` makes the next block a change, and a snapshot with no usable rate
    /// detects only start and stop, not jumps (it cannot predict one).
    #[test]
    fn a_playhead_watch_resets_and_degrades_without_a_rate() {
        let mut w = PlayheadWatch::new();
        let step = 512.0 / 24_000.0;
        w.observe(&block(true, 0.0), 512);
        assert!(!w.observe(&block(true, step), 512));
        w.reset();
        // Exactly where playing would have put it: only the reset makes it a change.
        assert!(
            w.observe(&block(true, 2.0 * step), 512),
            "first block after reset"
        );

        let blind = |playing, beats| block(playing, beats).with_sample_rate(0.0);
        let mut w = PlayheadWatch::new();
        w.observe(&blind(true, 0.0), 512);
        assert!(
            !w.observe(&blind(true, 0.02), 512),
            "no prediction, no jump"
        );
        assert!(w.observe(&blind(false, 0.02), 512), "stop is still seen");
    }
}
