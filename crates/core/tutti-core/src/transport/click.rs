//! Metronome click node — click sounds synced to the transport.
//!
//! The click node reads the playhead **per sample** from the
//! [`TransportClock`](super::TransportClock)'s two beat ports, the live-session
//! flags off the concrete [`Transport`]'s shared atomics, and its own settings
//! (volume, meter, mode) from [`ClickSettings`].
//!
//! # Why the beat arrives on ports, not through the transport's atomic
//!
//! The transport's `beat` atomic is the clock's *writeback*: stored once, at the
//! end of the clock's block, as the beat the **next** block starts on. A node
//! reading it gets one value per block, so a click could only ever start on a
//! block boundary — up to a whole block (64 frames, 1.3 ms at 48 kHz) of jitter
//! on every beat. Worse, which block that value belongs to depended on whether
//! fundsp happened to order the clock before or after the click: two unconnected
//! nodes have no defined order, so it read either this block's start or the
//! next one's.
//!
//! Taking the beat as an input fixes both. The edge makes the clock run first,
//! and the ports carry the playhead of every sample — including a loop wrap or
//! a seek landing mid-block — so an onset starts on the exact frame whose beat
//! first reaches it. It is the convention every other beat-driven node already
//! uses (see [`BEAT_PORTS`]). The cost is that the click must be **wired**:
//! with nothing on its inputs it reads beat 0 forever and clicks once.
//!
//! # Meter arrives here, not through the transport
//!
//! The metronome is the one audio-thread consumer of musical meter, and it gets
//! it through its own settings bundle rather than through the transport. That
//! keeps meter a layer *over* the engine: `TransportSettings`, `Timeline`, and
//! the graph know nothing about bars.

use super::state::{beat_from_ports, BEAT_PORTS};
use super::Transport;
use crate::{AtomicF32, AtomicU8, AudioUnit, BufferMut, BufferRef, Ordering, SignalFrame};
use std::sync::Arc;
use tutti_types::meter::{Meter, MeterMap};
use tutti_types::value::{Amplitude, Beat, Hz, Phase, PhaseIncrement, Samples, Seconds};
use tutti_types::RtPublish;

/// How far two beat onsets must differ to count as different beats.
///
/// The playhead is an accumulated `f64`, so an exact compare would retrigger on
/// drift alone. A thousandth of a quarter note is far below the shortest notated
/// beat any legal meter can express (a 64th note is 0.0625 quarters) and far
/// above the drift of a realistic session.
const ONSET_EPSILON: f64 = 1e-3;

/// Metronome operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum MetronomeMode {
    /// Metronome is disabled.
    #[default]
    Off,
    /// Only play during preroll count-in.
    PrerollOnly,
    /// Only play while recording.
    RecordingOnly,
    /// Always play when transport is running.
    Always,
}

impl From<u8> for MetronomeMode {
    fn from(value: u8) -> Self {
        debug_assert!(value <= 3, "invalid MetronomeMode discriminant: {value}");
        match value {
            1 => MetronomeMode::PrerollOnly,
            2 => MetronomeMode::RecordingOnly,
            3 => MetronomeMode::Always,
            _ => MetronomeMode::Off,
        }
    }
}

impl From<MetronomeMode> for u8 {
    #[inline]
    fn from(mode: MetronomeMode) -> u8 {
        mode as u8
    }
}

impl core::fmt::Display for MetronomeMode {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::PrerollOnly => "preroll only",
            Self::RecordingOnly => "recording only",
            Self::Always => "always",
        })
    }
}

/// Click-specific settings (volume, meter, mode).
///
/// Transport state (beat, playing, recording, preroll) comes from the
/// [`Transport`] the node holds.
///
/// **Shared as `Arc<ClickSettings>`, never cloned by value.** fundsp clones nodes
/// on every graph commit, so a settings field owned per-node would leave the app's
/// writes landing on an orphan copy — the hazard documented on
/// `tutti_plugin::host::node::InputSlot`. Holding one `Arc` is what makes a
/// `set_meter` from the UI thread visible to whichever clone the audio thread is
/// running.
#[repr(align(64))]
#[derive(Debug)]
pub struct ClickSettings {
    volume: AtomicF32,
    /// The project meter, driving both the click rate and the downbeat accent.
    ///
    /// An [`RtPublish`] rather than a packed atomic because a [`MeterMap`] is a
    /// `Vec`, not a scalar. Read once per block in [`ClickNode::process`], never
    /// per sample — the read is a guard acquire, far heavier than the plain
    /// atomic loads beside it.
    ///
    /// Wrapped in its own `Arc` so the *cell* can be shared, not just its
    /// contents: hosted plugins need the same meter for their transport
    /// snapshot, and handing them this handle means one publish reaches the
    /// metronome and every plugin at once. See [`meter_cell`](Self::meter_cell).
    meter: Arc<RtPublish<MeterMap>>,
    mode: AtomicU8,
}

impl ClickSettings {
    /// Default settings: mode [`MetronomeMode::Off`], half volume, 4/4.
    pub fn new() -> Self {
        Self {
            volume: AtomicF32::new(0.5),
            meter: Arc::new(RtPublish::new(MeterMap::default())),
            mode: AtomicU8::new(MetronomeMode::Off as u8),
        }
    }

    /// Set the click level, clamped to `0.0..=1.0` in [`Amplitude`] (linear,
    /// not dB).
    pub fn set_volume(&self, volume: impl Into<Amplitude>) {
        self.volume
            .store(volume.into().get().clamp(0.0, 1.0), Ordering::Release);
    }

    /// The click level, always within `0.0..=1.0`.
    pub fn volume(&self) -> Amplitude {
        Amplitude(self.volume.load(Ordering::Acquire))
    }

    /// Publish a new meter. Lock-free; visible to the audio thread on its next
    /// block.
    pub fn set_meter(&self, meter: Arc<MeterMap>) {
        self.meter.publish(meter);
    }

    /// The meter in force. Prefer calling this once per block.
    ///
    /// A borrow, not an owning handle — see [`RtPublish`] for why that is the
    /// whole point on the audio thread. Hold it for the block you are rendering
    /// and no longer.
    pub fn meter(&self) -> tutti_types::RtRef<'_, MeterMap> {
        self.meter.read()
    }

    /// The shared meter cell, for other subsystems that must see the same value.
    ///
    /// Hosted plugins carry the meter in their transport snapshot; giving them
    /// this handle rather than a copy means [`set_meter`](Self::set_meter)
    /// reaches them too, with no second publish path to keep in sync.
    pub fn meter_cell(&self) -> Arc<RtPublish<MeterMap>> {
        Arc::clone(&self.meter)
    }

    /// Choose when the metronome sounds. Takes effect on the audio thread's
    /// next block.
    pub fn set_mode(&self, mode: MetronomeMode) {
        self.mode.store(mode.into(), Ordering::Release);
    }

    /// The mode in force.
    pub fn mode(&self) -> MetronomeMode {
        MetronomeMode::from(self.mode.load(Ordering::Acquire))
    }
}

impl Default for ClickSettings {
    fn default() -> Self {
        Self::new()
    }
}

/// Alias for [`ClickSettings`], for callers that name the metronome's shared
/// cell as state rather than settings.
pub type ClickState = ClickSettings;

/// The metronome, as a graph node.
///
/// Inputs: the beat, on [`BEAT_PORTS`] ports — wire them from the
/// [`TransportClock`](super::TransportClock)'s outputs 0 and 1 (on a native
/// graph, an [`EnvClock`](super::EnvClock)'s: the same samples). Outputs: the
/// click, stereo. The beat arrives as a signal so each onset starts on its exact frame.
///
/// Takes the live [`Transport`] concretely rather than a
/// [`Timeline`](super::Timeline): the
/// metronome's modes depend on `recording` / `in_preroll`, which are
/// live-session facts an offline timeline has no answer for. It only ever
/// reads — no transport control.
///
/// A click node does end up inside offline-cloned nets (the clone copies every
/// node), but `Net::clone_isolated` repoints the output bus away from it, so its
/// samples go nowhere.
#[derive(Clone)]
pub struct ClickNode {
    transport: Transport,
    settings: Arc<ClickSettings>,
    sample_rate: crate::SampleRate,
    click_normal: Vec<f32>,
    click_accent: Vec<f32>,
    click_pos: usize,
    is_accent: bool,
    /// Timeline position of the notated beat currently sounding, or `None` when
    /// nothing has been clicked yet.
    ///
    /// A position rather than a running index: an index has to be scaled by some
    /// `beat_length`, which changes across a meter change, so it is only unique
    /// within one segment. `Option` rather than a sentinel value, since every
    /// finite beat — including negative pre-roll — is a legitimate onset.
    last_click_onset: Option<Beat>,
    /// Where the notated beat latched in `last_click_onset` ends: the next
    /// onset, or an earlier meter change. While the playhead stays in
    /// `[last_click_onset, latched_until)` nothing can retrigger, so the
    /// per-sample onset check is two compares instead of a `bar_at`.
    ///
    /// **A cache of the meter, so it is dropped at every block** (see
    /// [`forget_latch`](Self::forget_latch)): a meter republished mid-beat must
    /// be seen within one block, as it was when the beat was read per block,
    /// not only once the playhead leaves a span the *old* meter drew.
    ///
    /// Meaningless while `last_click_onset` is `None`, and never read then.
    latched_until: Beat,
    /// The live-session flags as they were when this copy was isolated, or
    /// `None` on a live node, which reads them off `transport` every block.
    /// See `AudioUnit::isolate` below.
    frozen: Option<SessionFlags>,
}

/// The three live-session facts the metronome's modes gate on, captured at
/// fork time so a fork does not follow the live transport's play, count-in
/// and record state while it renders.
#[derive(Clone, Copy, Debug)]
struct SessionFlags {
    playing: bool,
    in_preroll: bool,
    recording: bool,
}

impl ClickNode {
    /// Build a click node reading `transport` and `settings`, with both click
    /// waveforms rendered for `sample_rate`.
    ///
    /// `settings` is taken as an `Arc` so a later `set_meter` / `set_volume`
    /// reaches whichever clone fundsp is running — see [`ClickSettings`].
    pub fn with_transport(
        transport: Transport,
        settings: Arc<ClickSettings>,
        sample_rate: impl Into<crate::SampleRate>,
    ) -> Self {
        let sample_rate = sample_rate.into();
        let click_normal = Self::generate_click(sample_rate, false);
        let click_accent = Self::generate_click(sample_rate, true);

        Self {
            transport,
            settings,
            sample_rate,
            click_normal,
            click_accent,
            click_pos: 0,
            is_accent: false,
            last_click_onset: None,
            latched_until: Beat(f64::NEG_INFINITY),
            frozen: None,
        }
    }

    /// One rendered click: a windowed sine, generated once and replayed.
    ///
    /// The three envelope times are `Seconds` like the total, so the
    /// relationship between them (attack, then hold, then a release that ends
    /// exactly at `CLICK_LEN`) is stated in one unit rather than five bare
    /// floats that happen to line up.
    fn generate_click(sample_rate: crate::SampleRate, is_accent: bool) -> Vec<f32> {
        /// Total click length.
        const CLICK_LEN: Seconds = Seconds(0.03);
        /// Fade-in, then full level until `HOLD_END`, then fade to zero.
        const ATTACK: Seconds = Seconds(0.001);
        const HOLD_END: Seconds = Seconds(0.02);

        // `to_samples_ceil`, not a truncating cast: this sizes a buffer, which
        // is the allocating case the three named roundings exist to
        // distinguish. Measured, the difference is one frame and only at rates
        // where 30 ms is not whole — 22050 Hz gives 661 vs 662, while 44.1/48/
        // 88.2/96 k are unchanged. Correctness of the *name*, not a fix for
        // anything audible.
        let num_samples = CLICK_LEN.to_samples_ceil(sample_rate).get();

        let freq = if is_accent { Hz(1200.0) } else { Hz(1000.0) };
        let accent_volume = if is_accent {
            Amplitude(1.0)
        } else {
            Amplitude(0.7)
        };
        let release_len = CLICK_LEN - HOLD_END;
        let phase_inc = PhaseIncrement::per_sample(freq, sample_rate);

        let mut phase = Phase::START;
        (0..num_samples)
            .map(|i| {
                let t = Samples(i).to_seconds(sample_rate);
                let env = if t < ATTACK {
                    t.get() / ATTACK.get()
                } else if t < HOLD_END {
                    1.0
                } else {
                    1.0 - (t - HOLD_END).get() / release_len.get()
                };
                let sample = phase.to_radians().sin() * env * accent_volume.get();
                phase = phase.advance(phase_inc);
                sample
            })
            .collect()
    }

    /// Whether the metronome should sound at all, given mode and live state.
    #[inline]
    fn should_play(&self) -> bool {
        match self.settings.mode() {
            MetronomeMode::Off => false,
            MetronomeMode::Always => self.playing(),
            MetronomeMode::PrerollOnly => self.in_preroll(),
            MetronomeMode::RecordingOnly => self.recording() && !self.in_preroll(),
        }
    }

    #[inline]
    fn playing(&self) -> bool {
        self.frozen
            .map_or_else(|| self.transport.motion.is_playing(), |f| f.playing)
    }

    #[inline]
    fn in_preroll(&self) -> bool {
        self.frozen
            .map_or_else(|| self.transport.settings.is_in_preroll(), |f| f.in_preroll)
    }

    #[inline]
    fn recording(&self) -> bool {
        self.frozen
            .map_or_else(|| self.transport.settings.is_recording(), |f| f.recording)
    }

    /// Silence the node and forget the last beat, so resuming re-triggers.
    ///
    /// Clears `is_accent` too: `advance_to` always rewrites it before the next
    /// sample is emitted, so leaving it stale is latent rather than live — but
    /// then `reset()` would not be a reset, and any future early-return path
    /// would turn that into a real bug.
    #[inline]
    fn go_silent(&mut self) {
        self.click_pos = 0;
        self.is_accent = false;
        self.last_click_onset = None;
    }

    /// Retrigger if `beat` has crossed into a new notated beat under `meter`.
    ///
    /// The index counts *notated* beats, not quarter notes: in 7/8 that is an
    /// eighth, so the metronome clicks seven times per bar rather than four.
    ///
    /// Called once per **sample**, so the common case — still inside the beat
    /// already latched — returns after two compares; `bar_at` runs only when the
    /// playhead leaves that span, i.e. about once per notated beat.
    ///
    /// Takes its mutable fields individually rather than `&mut self`: the meter
    /// arrives as a read lease borrowed from `self.settings`, so a whole-`self`
    /// mutable borrow would collide with it at every call site.
    #[inline]
    fn advance_to(
        last_click_onset: &mut Option<Beat>,
        latched_until: &mut Beat,
        click_pos: &mut usize,
        is_accent: &mut bool,
        meter: &MeterMap,
        beat: Beat,
    ) {
        if let Some(onset) = *last_click_onset {
            if beat >= onset && beat < *latched_until {
                return;
            }
        }

        let position = meter.bar_at(beat);

        // Identify the beat by the *onset it belongs to*, not by a running
        // count. A count would have to be scaled by some `beat_length`, and
        // `beat_length` changes across a `MeterChange` — so an index computed
        // with the current segment's scale is only valid within that segment,
        // and collides with indices from earlier segments (suppressing clicks)
        // the moment a meter change exists. The onset is unambiguous everywhere
        // and needs no global numbering.
        let onset = position.bar_start
            + position.signature.beat_length() * (position.beat.get() - 1) as f64;

        // Inequality, not `>`: a backward jump from a loop wrap must retrigger
        // too. The epsilon is for float drift in the accumulated playhead, well
        // below the shortest notated beat this meter can express.
        let changed = match *last_click_onset {
            Some(previous) => (onset - previous).get().abs() > ONSET_EPSILON,
            None => true,
        };

        if changed {
            *last_click_onset = Some(onset);
            *click_pos = 0;
            // The accent is the bar's downbeat, straight from the meter — never
            // a standalone every-N count, which drifts against the bar the
            // moment the time signature is not 4/4.
            *is_accent = position.is_downbeat();
        }

        // The latched beat ends at the next notated onset — or sooner, at a
        // meter change, which may fall mid-beat (the bar before a change is
        // simply short). Measured from the onset actually latched, so a
        // within-epsilon non-change keeps the span it already had.
        let latched = last_click_onset.unwrap_or(onset);
        let next_onset = latched + position.signature.beat_length();
        let changes = meter.changes();
        let next_change = changes
            .get(changes.partition_point(|c| c.beat <= latched))
            .map(|c| c.beat);
        *latched_until = match next_change {
            Some(change) if change < next_onset => change,
            _ => next_onset,
        };
    }

    /// Drop the cached span, so the next onset check consults the meter.
    ///
    /// Called at the top of every block — `tick` included, being a one-frame
    /// block — which is all it takes for a republished meter to reach the node:
    /// the meter lease is re-read per block anyway, and this is the only thing
    /// derived from it that outlives one.
    #[inline]
    fn forget_latch(&mut self) {
        self.latched_until = Beat(f64::NEG_INFINITY);
    }

    /// One sample of the click envelope, or silence once it has run out.
    ///
    /// Over the playback fields rather than `&mut self`, for the reason
    /// [`advance_to`](Self::advance_to) gives: the block loop holds the meter
    /// lease borrowed from `self.settings` while it calls this.
    #[inline]
    fn next_sample(
        click_normal: &[f32],
        click_accent: &[f32],
        click_pos: &mut usize,
        is_accent: bool,
        volume: Amplitude,
    ) -> f32 {
        let buffer = if is_accent {
            click_accent
        } else {
            click_normal
        };

        if *click_pos < buffer.len() {
            let sample = buffer[*click_pos] * volume.get();
            *click_pos += 1;
            sample
        } else {
            0.0
        }
    }
}

impl AudioUnit for ClickNode {
    /// The beat, whole then fraction — the clock's two outputs.
    fn inputs(&self) -> usize {
        BEAT_PORTS
    }

    fn outputs(&self) -> usize {
        2
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        if !self.should_play() {
            self.go_silent();
            output[0] = 0.0;
            output[1] = 0.0;
            return;
        }

        self.forget_latch();
        Self::advance_to(
            &mut self.last_click_onset,
            &mut self.latched_until,
            &mut self.click_pos,
            &mut self.is_accent,
            &self.settings.meter(),
            beat_from_ports(input[0], input[1]),
        );

        let sample = Self::next_sample(
            &self.click_normal,
            &self.click_accent,
            &mut self.click_pos,
            self.is_accent,
            self.settings.volume(),
        );
        output[0] = sample;
        output[1] = sample;
    }

    /// Per-block render, overriding the default per-sample `tick` loop.
    ///
    /// The default `AudioUnit::process` calls `tick` once per sample, which would
    /// put an `RtPublish::read` — a guard acquire, not a plain atomic read — on
    /// every one of ~2.8M samples per second. Hoisting the mode, transport flags,
    /// volume, and meter to once per buffer is the same shape `TransportClock`
    /// uses, and is what makes reading a `MeterMap` here affordable at all.
    ///
    /// The beat is **not** hoisted: it is read from the input ports every
    /// sample, so an onset starts on the frame whose beat first reaches it rather
    /// than on the next block boundary. That read is two port loads, not an
    /// atomic, and the onset test behind it is two compares except once per
    /// notated beat — see `advance_to`.
    ///
    /// This is *not* bit-identical to N calls of `tick` in general — `tick`
    /// re-reads the mode, the transport flags, and the volume per sample, so a UI
    /// write lands mid-block there and at the next block boundary here. That is
    /// the intended trade and the granularity is bounded: `Engine::process_segment`
    /// already chops the callback into `MAX_BUFFER_SIZE` chunks, so the worst-case
    /// lag is ~1.3 ms at 48 kHz. The two agree exactly whenever those settings are
    /// stable across the block — the beat may move freely — which is what the
    /// equivalence test pins.
    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        if !self.should_play() {
            self.go_silent();
            for channel in 0..2 {
                for i in 0..size {
                    output.set_f32(channel, i, 0.0);
                }
            }
            return;
        }

        self.forget_latch();
        let volume = self.settings.volume();
        // One lease for the whole block — `RtPublish::read` is never per sample.
        let meter = self.settings.meter();

        for i in 0..size {
            Self::advance_to(
                &mut self.last_click_onset,
                &mut self.latched_until,
                &mut self.click_pos,
                &mut self.is_accent,
                &meter,
                beat_from_ports(input.at_f32(0, i), input.at_f32(1, i)),
            );
            let sample = Self::next_sample(
                &self.click_normal,
                &self.click_accent,
                &mut self.click_pos,
                self.is_accent,
                volume,
            );
            output.set_f32(0, i, sample);
            output.set_f32(1, i, sample);
        }
    }

    /// Snapshot everything this node reads live, so a fork renders the
    /// metronome as it was set when it was taken: a fresh [`ClickSettings`]
    /// holding the current volume, mode and meter (the live `Arc` is shared by
    /// every clone and by the host that calls `set_volume`/`set_meter`), and
    /// the transport's play, count-in and record flags frozen at their current
    /// values. Nothing is written back to either; the node only ever read them.
    ///
    /// Runs on the control thread (it allocates, and `meter()`'s borrow is
    /// released before the fresh cell is built).
    fn isolate(&mut self) {
        let fresh = ClickSettings::new();
        fresh.volume.store(
            self.settings.volume.load(Ordering::Acquire),
            Ordering::Release,
        );
        fresh.set_mode(self.settings.mode());
        let meter = Arc::new(self.settings.meter().clone());
        fresh.set_meter(meter);
        self.settings = Arc::new(fresh);
        self.frozen = Some(SessionFlags {
            playing: self.transport.motion.is_playing(),
            in_preroll: self.transport.settings.is_in_preroll(),
            recording: self.transport.settings.is_recording(),
        });
    }

    fn reset(&mut self) {
        self.go_silent();
    }

    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        if (self.sample_rate.get() - sample_rate.get()).abs() > 0.1 {
            self.sample_rate = sample_rate;
            self.click_normal = Self::generate_click(sample_rate, false);
            self.click_accent = Self::generate_click(sample_rate, true);
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::CLICK_NODE_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    /// Both outputs [`Signal::Unknown`](crate::Signal::Unknown) — a generator whose samples fundsp
    /// cannot trace back to an input.
    ///
    /// Written out rather than left to a default, because there is no longer a
    /// default to leave it to: this is byte-for-byte what `AudioNode::route`
    /// supplied while this was `An<ClickNode>`, and `latency()` is *derived*
    /// from `route`, so an `Unknown` here is what keeps the click out of PDC's
    /// arithmetic exactly as before.
    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(self.outputs())
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::super::clock::split_beat;
    use super::super::{beats_per_sample, ClockLinks, TransportClock};
    use super::*;
    use crate::BufferVec;
    use tutti_types::meter::{BeatsPerBar, MeterChange, NoteValue, TimeSignature};

    /// Drive the real `Transport` rather than a mock: the click node needs
    /// recording/preroll, which only the live transport has, and a mock would
    /// just restate its fields.
    fn playing(t: &Transport) {
        let _ = t.motion.try_send(super::super::MotionEvent::Play);
        t.motion.drain();
    }

    /// One `tick`, as a `[left, right]` pair.
    ///
    /// [`AudioUnit::tick`] writes through an out-slice rather than returning a
    /// `Frame` the way [`AudioNode::tick`] did. The tests below assert on
    /// `output[0]` / `output[1]` and read better that way, so the shape is
    /// restored here instead of at eighteen call sites.
    ///
    /// The beat ports carry `transport.settings.beat()` — standing in for a
    /// wired clock — so a test positions the playhead with `set_beat` and the
    /// node sees exactly that on its inputs.
    fn tick(node: &mut ClickNode) -> [f32; 2] {
        let (whole, frac) = split_beat(node.transport.settings.beat());
        let mut out = [0.0f32; 2];
        AudioUnit::tick(node, &[whole, frac], &mut out);
        out
    }

    /// The beat ports for one block, holding `beat` on every frame.
    fn constant_beat(beat: Beat) -> BufferVec {
        let (whole, frac) = split_beat(beat);
        let mut ports = BufferVec::new(2);
        for i in 0..crate::MAX_BUFFER_SIZE {
            ports.set_f32(0, i, whole);
            ports.set_f32(1, i, frac);
        }
        ports
    }

    /// A clock at `bpm` starting at `start`, sharing nothing with any live
    /// transport — the source of a realistic, moving beat signal.
    fn clock_at(bpm: f64, start: f64, sample_rate: f64) -> TransportClock {
        let links = ClockLinks::bare(
            Arc::new(crate::AtomicF64::new(bpm)),
            Arc::new(crate::AtomicBool::new(false)),
        );
        TransportClock::new(links, sample_rate).starting_at(Beat(start))
    }

    fn make_click() -> (Transport, Arc<ClickSettings>, ClickNode) {
        let transport = Transport::new(44100.0);
        let settings = Arc::new(ClickSettings::new());
        let node = ClickNode::with_transport(transport.clone(), Arc::clone(&settings), 44100.0);
        (transport, settings, node)
    }

    #[test]
    fn test_click_node_silent_when_paused() {
        let (_, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::Always);

        // transport.playing is false by default
        let output = tick(&mut node);
        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    #[test]
    fn test_click_node_plays_on_beat() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);

        let mut found_nonzero = false;
        for _ in 0..100 {
            let output = tick(&mut node);
            if output[0] != 0.0 || output[1] != 0.0 {
                found_nonzero = true;
                break;
            }
        }
        assert!(found_nonzero, "Click should produce non-zero output");

        // Advance to beat 1
        transport.settings.set_beat(1.0);
        found_nonzero = false;
        for _ in 0..100 {
            let output = tick(&mut node);
            if output[0] != 0.0 || output[1] != 0.0 {
                found_nonzero = true;
                break;
            }
        }
        assert!(
            found_nonzero,
            "Click should produce non-zero output on new beat"
        );
    }

    #[test]
    fn test_click_plays_after_loop_wrap() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);

        // Advance to beat 7
        transport.settings.set_beat(7.0);
        let _ = tick(&mut node);
        assert_eq!(node.last_click_onset, Some(Beat(7.0)));

        // Simulate loop wrap: beat jumps backward from 7 to 4
        transport.settings.set_beat(4.0);
        let mut found_nonzero = false;
        for _ in 0..100 {
            let output = tick(&mut node);
            if output[0] != 0.0 {
                found_nonzero = true;
                break;
            }
        }
        assert_eq!(
            node.last_click_onset,
            Some(Beat(4.0)),
            "Should reset to beat 4 after loop wrap"
        );
        assert!(found_nonzero, "Click should play after loop wrap");
    }

    /// The accent is the bar's downbeat, taken from the meter — not a fixed
    /// every-N count, which would ignore the project's time signature.
    #[test]
    fn accent_follows_the_meter_downbeat() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::Always);

        let accents_at = |node: &mut ClickNode, beat: f64| {
            transport.settings.set_beat(beat);
            node.reset();
            let _ = tick(node);
            node.is_accent
        };

        // Default 4/4: accent every 4 quarter notes.
        assert!(accents_at(&mut node, 0.0));
        assert!(!accents_at(&mut node, 1.0));
        assert!(accents_at(&mut node, 4.0));

        // 3/4: the accent moves to every 3 quarters. A fixed count of 4 would
        // drift against the bar here.
        settings.set_meter(Arc::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            TimeSignature::new(BeatsPerBar::new(3), NoteValue::QUARTER),
        )])));
        assert!(accents_at(&mut node, 3.0));
        assert!(!accents_at(&mut node, 4.0));
        assert!(accents_at(&mut node, 6.0));
    }

    /// In 7/8 the notated beat is an eighth, so a bar holds seven clicks across
    /// 3.5 quarter notes — not four clicks on the quarters.
    #[test]
    fn click_rate_follows_the_notated_beat() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::Always);
        settings.set_meter(Arc::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH),
        )])));

        // Half a quarter note apart is a full notated beat in 7/8, so these are
        // distinct clicks.
        transport.settings.set_beat(0.0);
        let _ = tick(&mut node);
        assert_eq!(node.last_click_onset, Some(Beat(0.0)));
        assert!(node.is_accent, "beat 0 is the downbeat of bar 1");

        transport.settings.set_beat(0.5);
        let _ = tick(&mut node);
        assert_eq!(
            node.last_click_onset,
            Some(Beat(0.5)),
            "an eighth is one notated beat"
        );
        assert!(!node.is_accent, "beat 2 of the bar is not accented");

        // The next bar starts at 3.5 quarters, not 7.
        transport.settings.set_beat(3.5);
        let _ = tick(&mut node);
        assert!(node.is_accent, "3.5 quarters is the downbeat of bar 2");
    }

    /// Clicks must stay distinct across a meter change.
    ///
    /// A running index scaled by the *current* segment's `beat_length` cannot
    /// do this: `beat_length` changes at a `MeterChange`, so indices from a 7/8
    /// segment collide with indices from a following 4/4 segment and silently
    /// suppress clicks. Identifying the beat by its onset position removes the
    /// scale entirely.
    #[test]
    fn clicks_stay_distinct_across_a_meter_change() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::Always);
        // 7/8 (eighth-note beats) for three bars, then 4/4 (quarter-note beats).
        settings.set_meter(Arc::new(MeterMap::new([
            MeterChange::new(
                Beat(0.0),
                TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH),
            ),
            MeterChange::new(Beat(10.5), TimeSignature::default()),
        ])));

        // Walk every notated beat across the change and collect the onsets the
        // node actually latched.
        let mut onsets = Vec::new();
        let mut beat = 0.0;
        while beat < 14.5 {
            transport.settings.set_beat(beat);
            let _ = tick(&mut node);
            if let Some(onset) = node.last_click_onset {
                if onsets.last() != Some(&onset) {
                    onsets.push(onset);
                }
            }
            // Step by whichever notated beat is in force here.
            beat += if beat < 10.5 { 0.5 } else { 1.0 };
        }

        // Every latched onset must be distinct and strictly increasing — a
        // collision would show up as a missing entry.
        for pair in onsets.windows(2) {
            assert!(
                pair[1] > pair[0],
                "onsets must strictly increase across the change, got {:?} then {:?}",
                pair[0],
                pair[1]
            );
        }
        // 21 eighths before the change (0.0..10.5) + 4 quarters after.
        assert_eq!(
            onsets.len(),
            25,
            "every notated beat must click exactly once"
        );
        assert!(onsets.contains(&Beat(10.5)), "the change begins a bar");
    }

    /// A meter change that lands *inside* a notated beat still clicks its
    /// downbeat, on its frame.
    ///
    /// Within a block the onset check skips `bar_at` while the playhead stays in
    /// the latched beat's span, so that span must end at the change rather than
    /// at the next onset of the old meter — a change always begins a bar, and
    /// here it begins one half-way through a quarter note. Driven through
    /// `process` with the change mid-block, because the span is dropped at every
    /// block boundary and `tick` is a one-frame block: a `tick`-driven version
    /// of this test passes with or without the cap.
    ///
    /// Mutation: ending the latched span at `onset + beat_length` alone, without
    /// the cap at the next change, keeps beat 2.5 inside `[2.0, 3.0)` for the
    /// rest of the block, so the downbeat does not click and every assertion
    /// fails.
    #[test]
    fn a_mid_beat_meter_change_clicks_its_downbeat() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);
        settings.set_meter(Arc::new(MeterMap::new([
            MeterChange::new(Beat(0.0), TimeSignature::default()),
            MeterChange::new(
                Beat(2.5),
                TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH),
            ),
        ])));

        // Frame `i` carries beat `2 + i/100`, so the change at 2.5 is frame 50.
        let mut ports = BufferVec::new(2);
        for i in 0..crate::MAX_BUFFER_SIZE {
            let (whole, frac) = split_beat(Beat(2.0 + i as f64 / 100.0));
            ports.set_f32(0, i, whole);
            ports.set_f32(1, i, frac);
        }
        // Beat 2's click has already played out, so only the change can sound.
        node.last_click_onset = Some(Beat(2.0));
        node.click_pos = node.click_normal.len();

        let mut out = BufferVec::new(2);
        node.process(64, &ports.buffer_ref(), &mut out.buffer_mut());

        assert_eq!(
            node.last_click_onset,
            Some(Beat(2.5)),
            "the change's downbeat is a new onset"
        );
        assert!(node.is_accent, "and it is a downbeat");
        let first_sound = (0..64).position(|i| out.at_f32(0, i) != 0.0);
        // The click's first frame is exactly zero, so it sounds one frame after
        // its onset — see `click_onset_is_sample_accurate_within_the_block`.
        assert_eq!(first_sound, Some(51), "clicked on the change's frame");
    }

    /// A meter published while a beat is latched is in force from the next
    /// block, not from whenever the playhead leaves the span the old meter drew.
    ///
    /// Mutation: removing `forget_latch` from `process` keeps the 4/4 span
    /// `[2.0, 3.0)` across the publish, so the node still holds onset 2.0 after
    /// the second block and fails.
    #[test]
    fn a_republished_meter_is_seen_within_one_block() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::Always);

        // Frame `i` of a block starting at `start` carries beat `start + i/100`.
        let block_at = |start: f64| {
            let mut ports = BufferVec::new(2);
            for i in 0..crate::MAX_BUFFER_SIZE {
                let (whole, frac) = split_beat(Beat(start + i as f64 / 100.0));
                ports.set_f32(0, i, whole);
                ports.set_f32(1, i, frac);
            }
            ports
        };
        let mut out = BufferVec::new(2);

        // 4/4 (the default): 2.00..2.10 all belong to the quarter at 2.0.
        node.process(10, &block_at(2.0).buffer_ref(), &mut out.buffer_mut());
        assert_eq!(node.last_click_onset, Some(Beat(2.0)));

        // 3/16: sixteenth-note beats, three to a 0.75-quarter bar. Beat 2.64
        // falls in the sixteenth starting at 2.5 — inside the old span.
        settings.set_meter(Arc::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            TimeSignature::new(BeatsPerBar::new(3), NoteValue::SIXTEENTH),
        )])));
        node.process(10, &block_at(2.64).buffer_ref(), &mut out.buffer_mut());
        assert_eq!(
            node.last_click_onset,
            Some(Beat(2.5)),
            "the new meter's beat must be the one latched"
        );
    }

    /// Pre-roll sits at negative beats, so the accent must be derived from the
    /// meter rather than from a cast — `(beat as u32)` turns -1 into
    /// 4294967295 and accents an arbitrary beat.
    #[test]
    fn negative_beats_do_not_wrap() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::Always);

        // One bar before the start: a downbeat, and no panic or wrap.
        transport.settings.set_beat(-4.0);
        let _ = tick(&mut node);
        assert!(node.is_accent, "-4.0 in 4/4 is the downbeat of bar 0");

        transport.settings.set_beat(-3.0);
        let _ = tick(&mut node);
        assert!(!node.is_accent, "-3.0 is beat 2 of bar 0");
    }

    /// `process` must render exactly what a `tick` loop would, since it exists
    /// only to hoist the per-block reads.
    ///
    /// The beat *moves* through the block and crosses an onset mid-block, so
    /// this pins the per-sample onset path as well as the steady one: a
    /// `process` that read the beat once per block would start the click on
    /// frame 0 and diverge from `tick` there.
    #[test]
    fn process_matches_tick_sample_for_sample() {
        let (transport, settings, mut block_node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);

        // 44.1 kHz at 120 BPM is 1/22050 beat per frame, so starting 20 frames
        // before beat 2 puts that onset at frame 20 of the block.
        let mut clock = clock_at(120.0, 2.0 - 20.0 / 22_050.0, 44_100.0);
        let mut beats = BufferVec::new(2);
        const N: usize = 64;
        clock.process(N, &BufferRef::new(&[]), &mut beats.buffer_mut());

        // Latch the beat before it, as a transport rolling into this block
        // would have, with its click already played out, so the onset at beat 2
        // is the only sound in the block.
        block_node.last_click_onset = Some(Beat(1.0));
        block_node.click_pos = block_node.click_normal.len();
        let mut tick_node = block_node.clone();

        let mut buffer = BufferVec::new(2);
        block_node.process(N, &beats.buffer_ref(), &mut buffer.buffer_mut());
        assert!(
            (0..N).any(|i| buffer.at_f32(0, i) != 0.0),
            "the onset must fall inside the block, or this compares silence"
        );

        for i in 0..N {
            let mut expected = [0.0f32; 2];
            AudioUnit::tick(
                &mut tick_node,
                &[beats.at_f32(0, i), beats.at_f32(1, i)],
                &mut expected,
            );
            assert_eq!(
                buffer.at_f32(0, i),
                expected[0],
                "left channel diverged at sample {i}"
            );
            assert_eq!(
                buffer.at_f32(1, i),
                expected[1],
                "right channel diverged at sample {i}"
            );
        }
    }

    /// A click starts on the frame where the playhead reaches its onset — not
    /// on the next block boundary — at more than one block size.
    ///
    /// D8 in design doc 013: the node read one beat per block from the clock's
    /// writeback, so every click started on a block boundary, up to a block late.
    ///
    /// The expected frame is derived from tempo arithmetic alone, not from the
    /// node: the clock emits `start + i·bps` on frame `i` (emit, then advance),
    /// so the first frame at or past the onset is `ceil((onset − start) / bps)`.
    /// `±1` absorbs the `f32` fraction port rounding across that boundary.
    ///
    /// Mutation: reading the beat from frame 0 of the ports for the whole block
    /// (the old once-per-block read) moves the onset to the next block boundary
    /// and fails at both block sizes: frame 2112 at 64 and 2120 at 40, against
    /// 2103. (A second size that shared a boundary with the first would prove
    /// nothing more, so 40 is chosen to land elsewhere.)
    #[test]
    fn click_onset_is_sample_accurate_within_the_block() {
        const SR: f64 = 48_000.0;
        const BPM: f64 = 137.0;
        const START: f64 = 0.9;
        const ONSET: f64 = 1.0;

        for block in [64usize, 40] {
            let transport = Transport::new(SR);
            playing(&transport);
            let settings = Arc::new(ClickSettings::new());
            settings.set_mode(MetronomeMode::Always);
            settings.set_volume(1.0);
            let mut node = ClickNode::with_transport(transport, settings, SR);
            // The click's first frame is exactly zero (`sin(0)` under a zero
            // attack) and its second is not, so the onset is one before the
            // first non-zero frame. Asserted, since the test leans on it.
            assert_eq!(node.click_normal[0], 0.0);
            assert_ne!(node.click_normal[1], 0.0);
            // As if the transport had rolled through beat 0 already: the click
            // under test is the one at beat 1, and beat 0's has played out.
            node.last_click_onset = Some(Beat(0.0));
            node.click_pos = node.click_normal.len();

            let bps = beats_per_sample(BPM, SR).get();
            let expected = ((ONSET - START) / bps).ceil() as usize;
            assert_ne!(
                expected % block,
                0,
                "the onset must land mid-block, or a block-quantised click passes"
            );

            let mut clock = clock_at(BPM, START, SR);
            let mut left = Vec::new();
            while left.len() < expected + 2 * block {
                let mut beats = BufferVec::new(2);
                clock.process(block, &BufferRef::new(&[]), &mut beats.buffer_mut());
                let mut out = BufferVec::new(2);
                node.process(block, &beats.buffer_ref(), &mut out.buffer_mut());
                left.extend((0..block).map(|i| out.at_f32(0, i)));
            }

            let first_sound = left
                .iter()
                .position(|s| *s != 0.0)
                .expect("the click at beat 1 must sound");
            let onset = first_sound - 1;
            assert!(
                onset.abs_diff(expected) <= 1,
                "block {block}: click started at frame {onset}, the beat reaches \
                 {ONSET} at frame {expected}"
            );
        }
    }

    /// Eight blocks of the metronome, hashed bit-for-bit against the render the
    /// node produced while it was an `An<ClickNode>`.
    ///
    /// # What this pins, and why a hash
    ///
    /// The `impl AudioNode` → `impl AudioUnit` rewrite (graph plan PR 4b) had to
    /// be an *identity*: the same samples, not merely samples that still pass
    /// the behavioural tests above. Those tests check onsets, accents and mode
    /// gating — every one of them would pass a click whose envelope had drifted
    /// by an LSB, and none of them would name it.
    ///
    /// The reference figure was captured by running this exact loop against
    /// `An(ClickNode)` on the commit before the rewrite: FNV-1a over the raw
    /// `f32` bits of all 1,024 samples, `0xE011_43CA_E6D4_ECD5` both before and
    /// after. A hash rather than a 1,024-entry array because the array would be
    /// unreadable and unmaintained, while any single-bit difference moves it.
    ///
    /// # Mutation-tested
    ///
    /// Verified to fail, not merely to pass: scaling one sample by `1.0 + f32::EPSILON`
    /// changes the digest. It also fails if `process`'s per-block hoisting is
    /// undone, if `generate_click`'s envelope changes, or if the beat schedule
    /// below is edited — all of which is the point. Recompute the constant ONLY
    /// with a deliberate, argued DSP change.
    ///
    /// [`get_id`](AudioUnit::get_id) is pinned separately by
    /// [`click_node_id_survived_the_audionode_rewrite`], because it feeds `ping`
    /// and so seeds phase for the whole graph — a change there is inaudible in
    /// this fixed-seed render and audible everywhere else.
    #[test]
    fn render_is_bit_identical_to_the_audionode_era() {
        /// FNV-1a over the little-endian `f32` bits, in emission order.
        ///
        /// Gated like its one use below, or MSVC builds see it as dead code.
        #[cfg(not(target_env = "msvc"))]
        fn digest(samples: &[f32]) -> u64 {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for s in samples {
                for byte in s.to_le_bytes() {
                    h ^= u64::from(byte);
                    h = h.wrapping_mul(0x0000_0100_0000_01b3);
                }
            }
            h
        }

        let transport = Transport::new(48_000.0);
        playing(&transport);
        let settings = Arc::new(ClickSettings::new());
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);
        let mut node =
            ClickNode::with_transport(transport.clone(), Arc::clone(&settings), 48_000.0);

        // Eight 64-frame blocks, the beat advancing an eighth note per block, so
        // onsets fire, the accent alternates, and a whole click envelope plays
        // out across block boundaries.
        let mut rendered = Vec::with_capacity(8 * 64 * 2);
        //
        // The beat is held constant across each block on the ports. That is the
        // schedule the reference was captured with — the node then read one
        // beat per block from the transport — so the same samples must come
        // out now that it reads the beat per frame.
        for block in 0..8 {
            let beats = constant_beat(Beat(f64::from(block) * 0.5));
            let mut buffer = BufferVec::new(2);
            node.process(64, &beats.buffer_ref(), &mut buffer.buffer_mut());
            for i in 0..64 {
                rendered.push(buffer.at_f32(0, i));
                rendered.push(buffer.at_f32(1, i));
            }
        }

        assert_eq!(rendered.len(), 1_024);
        assert!(
            rendered.iter().any(|s| *s != 0.0),
            "a render of pure silence would hash stably and prove nothing"
        );
        // The digest pins *this crate's* DSP against a refactor. It cannot pin
        // it across C runtimes, and on MSVC it does not: Windows renders
        // 0x3366_2B99_B2A6_EBB5 where glibc and Apple libm both render the
        // constant below.
        //
        // Which operation differs is settled by elimination rather than guessed.
        // Every step in `generate_click` — the `to_seconds` divide, the envelope
        // divides and subtract, `to_radians`, the two multiplies, the phase add
        // — is one of the operations IEEE-754 requires to be correctly rounded,
        // so each is bit-identical on any conforming platform. `sin` is the only
        // one the standard does not specify; it is quality-of-implementation per
        // libm, and MSVC's CRT disagrees with glibc in the last ulp.
        //
        // So asserting the digest on Windows would test the C runtime, not the
        // metronome. The guard runs where it means something, and the portable
        // properties above — length, and that the render is not silence — are
        // asserted everywhere.
        #[cfg(not(target_env = "msvc"))]
        assert_eq!(
            digest(&rendered),
            0xE011_43CA_E6D4_ECD5,
            "the metronome no longer renders what `An<ClickNode>` rendered"
        );
    }

    /// `get_id` still returns what `AudioNode::ID` did.
    ///
    /// Not cosmetic: `get_id` is what [`AudioUnit::ping`] mixes into the graph
    /// hash, which seeds every pseudorandom phase in the net. Repacking the
    /// five-byte `"Click"` literal into the crate's usual eight-byte mnemonic
    /// convention would be a silent, audible change, and the render test above
    /// cannot see it — its own output has no seeded randomness.
    #[test]
    fn click_node_id_survived_the_audionode_rewrite() {
        let (_, _, node) = make_click();
        assert_eq!(
            node.get_id(),
            0x0043_6c69_636b,
            "`\"Click\"` was `AudioNode::ID`; changing it reseeds the graph hash"
        );
    }

    #[test]
    fn test_preroll_only_mode() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::PrerollOnly);
        settings.set_volume(1.0);

        // Should be silent when not in preroll
        let output = tick(&mut node);
        assert_eq!(output[0], 0.0);

        // Enable preroll - should play
        transport.settings.set_in_preroll(true);
        node.reset();
        let mut found_nonzero = false;
        for _ in 0..100 {
            let output = tick(&mut node);
            if output[0] != 0.0 {
                found_nonzero = true;
                break;
            }
        }
        assert!(found_nonzero, "Click should play during preroll");
    }

    #[test]
    fn test_recording_only_mode() {
        let (transport, settings, mut node) = make_click();
        playing(&transport);
        settings.set_mode(MetronomeMode::RecordingOnly);
        settings.set_volume(1.0);

        // Should be silent when not recording
        let output = tick(&mut node);
        assert_eq!(output[0], 0.0);

        // Enable recording - should play
        transport.settings.set_recording(true);
        node.reset();
        let mut found_nonzero = false;
        for _ in 0..100 {
            let output = tick(&mut node);
            if output[0] != 0.0 {
                found_nonzero = true;
                break;
            }
        }
        assert!(found_nonzero, "Click should play during recording");

        // In preroll while recording - should NOT play
        transport.settings.set_in_preroll(true);
        node.reset();
        let output = tick(&mut node);
        assert_eq!(
            output[0], 0.0,
            "Click should not play during preroll in RecordingOnly mode"
        );
    }

    /// The beat pair of a playhead moving at 20 beats a second (48 kHz), so a
    /// snapshot render crosses several clicks and a bar line.
    fn fast_beat(ch: usize, frame: usize) -> f32 {
        let (whole, frac) = split_beat(Beat(frame as f64 / 2_400.0));
        if ch == 0 {
            whole
        } else {
            frac
        }
    }

    /// A fork of the click renders the metronome as it was set when it was
    /// taken: volume, mode and meter come from a fresh `ClickSettings`, and
    /// the play flag is frozen, so none of the four live moves below reaches
    /// it (and each is heard by a fork taken after it).
    ///
    /// Mutations (run): drop the `self.settings = Arc::new(fresh)` line →
    /// volume, mode and meter fail; drop `self.frozen = Some(..)` → "play
    /// state" fails; build the fresh settings with `ClickSettings::new()`'s
    /// defaults instead of the current values → the fork is silent (mode
    /// Off), so every control fails as inaudible.
    #[test]
    fn isolate_snapshots_settings_and_session_flags() {
        tutti_graph::contract::IsolateRow::new("ClickNode", || {
            let (transport, settings, node) = make_click();
            playing(&transport);
            settings.set_mode(MetronomeMode::Always);
            settings.set_volume(0.5);
            node
        })
        .input(fast_beat)
        .control("volume", |n| n.settings.set_volume(1.0))
        .control("mode", |n| n.settings.set_mode(MetronomeMode::Off))
        .control("meter", |n| {
            n.settings
                .set_meter(Arc::new(MeterMap::new([MeterChange::new(
                    Beat(0.0),
                    TimeSignature::new(BeatsPerBar::new(3), NoteValue::QUARTER),
                )])))
        })
        .control("play state", |n| {
            let _ = n
                .transport
                .motion
                .try_send(super::super::MotionEvent::Stop {
                    fade: super::super::FadeOut::Immediate,
                });
            n.transport.motion.drain();
        })
        .check();
    }
}
