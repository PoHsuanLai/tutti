//! Metronome click node — click sounds synced to the transport.
//!
//! The click node is a graph node: it reads the playhead
//! **per frame** from its block's [`Env`] — walked piece by piece across the
//! block's transport changes, with the host's own clock
//! ([`Env::for_each_beat`](tutti_graph::Env::for_each_beat)) — the play and record state from
//! the same pieces, the count-in flag off the transport's shared cell, and its
//! own settings (volume, meter, mode) from [`ClickSettings`].
//!
//! # Why the beat comes from `Env`, not the transport's atomic
//!
//! The transport's `beat` atomic is the clock's *writeback*: stored once per
//! block, as the beat the **next** block starts on. A node reading it gets
//! one value per block, so a click could only ever start on a block boundary
//! — up to a whole block of jitter on every beat. The
//! block's `Env` carries the transport at its first frame and every change
//! inside it (a start, a stop, a seek, a tempo or loop edit, each on its
//! frame), so the node computes the beat of every frame — a loop wrap or a
//! seek landing mid-block included — and an onset starts on the exact frame
//! whose beat first reaches it. A start or a stop inside a block gates the
//! click on its frame too, not at the block's start.
//!
//! Until its port to `tutti_graph::Node` the beat arrived on two input ports from a clock
//! node, and the click had to be wired to one; it now needs no wiring.
//!
//! # Meter arrives here, not through the transport
//!
//! The metronome is the one audio-thread consumer of musical meter, and it gets
//! it through its own settings bundle rather than through the transport. That
//! keeps meter a layer *over* the engine: `TransportSettings`, `Timeline`, and
//! the graph know nothing about bars.

use super::beat_walk::piece_beats;
use super::Transport;
use crate::{AtomicBool, AtomicF32, AtomicU8, Ordering};
use std::sync::Arc;
use tutti_graph::{
    Cx, Env, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare,
    Shape, Status,
};
use tutti_types::meter::{Meter, MeterMap};
use tutti_types::value::{Amplitude, Beat, Hz, Phase, PhaseIncrement, Samples, Seconds};
use tutti_types::{ChannelLayout, RtPublish, Tail};

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
/// Transport state (beat, playing, recording) comes from the node's block
/// [`Env`]; the count-in flag from the [`Transport`] it was built over.
///
/// **Shared as `Arc<ClickSettings>`, never cloned by value**: it is the
/// node's controls ([`ClickNode`]'s `IntoNode`), the one cell a host's
/// `set_volume` / `set_meter` / `set_mode` and the running node both hold. A
/// fork of the node gets a fresh one holding the values at the fork
/// ([`snapshot`](Self::snapshot)).
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

    /// Sets the click level, clamped to `0.0..=1.0` in [`Amplitude`] (linear,
    /// not dB).
    pub fn set_volume(&self, volume: impl Into<Amplitude>) {
        self.volume
            .store(volume.into().get().clamp(0.0, 1.0), Ordering::Release);
    }

    /// The click level, always within `0.0..=1.0`.
    pub fn volume(&self) -> Amplitude {
        Amplitude(self.volume.load(Ordering::Acquire))
    }

    /// Publishes a new meter. Lock-free; visible to the audio thread on its next
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

    /// A fresh cell holding this one's volume, mode and meter now, sharing
    /// nothing with it: what a fork of the metronome renders with. Control
    /// thread (it allocates; the meter's borrow is released before the fresh
    /// cell is built).
    pub fn snapshot(&self) -> ClickSettings {
        let fresh = ClickSettings::new();
        fresh
            .volume
            .store(self.volume.load(Ordering::Acquire), Ordering::Release);
        fresh.set_mode(self.mode());
        let meter = Arc::new(self.meter().clone());
        fresh.set_meter(meter);
        fresh
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
/// No inputs; the click, stereo, on its two outputs. The beat of each frame
/// comes from the block's [`Env`] (see the module docs), so each onset
/// starts on its exact frame and nothing has to be wired into the node.
///
/// Its controls are its [`ClickSettings`] (`IntoNode::Controls`), the cell it
/// was built with. The count-in flag (`PrerollOnly`, `RecordingOnly`) is read
/// off the [`Transport`] it was built over, once per block: a live-session
/// fact the graph's transport does not carry. Playing and recording are the
/// block's, piece by piece, so an export's metronome follows the export's
/// transport.
///
/// A fork (an export, a live duplicate) renders the metronome as it was set
/// at the fork: a [`snapshot`](ClickSettings::snapshot) of the settings and
/// the count-in flag as it stood, sharing nothing with the live node.
#[derive(Clone)]
pub struct ClickNode {
    settings: Arc<ClickSettings>,
    /// The transport's count-in flag (`TransportSettings::in_preroll`), or a
    /// fork's own copy of it.
    preroll: Arc<AtomicBool>,
    /// The rate `click_normal` and `click_accent` were rendered at; `None`
    /// until the node is first prepared.
    sample_rate: Option<crate::SampleRate>,
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
    /// per-frame onset check is two compares instead of a `bar_at`.
    ///
    /// **A cache of the meter, so it is dropped at every block** (see
    /// [`forget_latch`](Self::forget_latch)): a meter republished mid-beat must
    /// be seen within one block, not only once the playhead leaves a span the
    /// *old* meter drew.
    ///
    /// Meaningless while `last_click_onset` is `None`, and never read then.
    latched_until: Beat,
}

impl ClickNode {
    /// A click node reading the count-in flag of `transport` and `settings`.
    /// Its click waveforms are rendered when it is prepared, at the graph's
    /// rate.
    ///
    /// `settings` is taken as an `Arc` so a later `set_meter` / `set_volume`
    /// reaches the running node: they are its controls.
    pub fn new(transport: &Transport, settings: Arc<ClickSettings>) -> Self {
        Self {
            settings,
            preroll: Arc::clone(&transport.settings.in_preroll),
            sample_rate: None,
            click_normal: Vec::new(),
            click_accent: Vec::new(),
            click_pos: 0,
            is_accent: false,
            last_click_onset: None,
            latched_until: Beat(f64::NEG_INFINITY),
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

    /// Whether the metronome sounds under `mode`, over a piece of a block
    /// whose transport is `t`, with the count-in flag at `in_preroll`.
    #[inline]
    fn should_play(mode: MetronomeMode, t: &tutti_graph::Transport, in_preroll: bool) -> bool {
        match mode {
            MetronomeMode::Off => false,
            MetronomeMode::Always => t.playing,
            MetronomeMode::PrerollOnly => in_preroll,
            MetronomeMode::RecordingOnly => t.recording() && !in_preroll,
        }
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
    /// Called once per **frame**, so the common case — still inside the beat
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
    /// Called at the top of every block, which is all it takes for a
    /// republished meter to reach the node: the meter lease is re-read per
    /// block anyway, and this is the only thing derived from it that outlives
    /// one.
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

    /// Render `env`'s block into `left` and `right`.
    ///
    /// The mode, the count-in flag, the volume and the meter are read once
    /// per block — a UI write lands at the next block, never mid-block, and
    /// the meter's `RtPublish::read` (a guard acquire) is never per frame.
    /// The beat is per frame, and the play and record state per piece of the
    /// block ([`Env::segments`]): a stop inside the block silences the click
    /// from its frame, and a start sounds the beat it lands on, on its frame.
    fn render(&mut self, env: &Env, left: &mut [f32], right: &mut [f32]) {
        let mode = self.settings.mode();
        let in_preroll = self.preroll.load(Ordering::Acquire);
        let volume = self.settings.volume();
        self.forget_latch();
        for (start, piece) in env.segments() {
            if !Self::should_play(mode, &piece.transport, in_preroll) {
                self.go_silent();
                let range = start.index()..start.index() + piece.block_len.get();
                left[range.clone()].fill(0.0);
                right[range].fill(0.0);
                continue;
            }
            // One lease for the piece — `RtPublish::read` is never per frame.
            let meter = self.settings.meter();
            let Self {
                click_normal,
                click_accent,
                click_pos,
                is_accent,
                last_click_onset,
                latched_until,
                ..
            } = self;
            piece_beats(start, &piece, |i, beat| {
                Self::advance_to(
                    last_click_onset,
                    latched_until,
                    click_pos,
                    is_accent,
                    &meter,
                    beat,
                );
                let sample =
                    Self::next_sample(click_normal, click_accent, click_pos, *is_accent, volume);
                left[i] = sample;
                right[i] = sample;
            });
        }
    }
}

impl Node for ClickNode {
    /// No inputs, the click in stereo. A generator whose sound depends on
    /// the transport rather than an input, so [`Tail::Unbounded`]: never
    /// skipped as silent.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO).with_tail(Tail::Unbounded)
    }

    /// Render the two click waveforms at the prepared rate (control thread:
    /// this allocates), unless they already are.
    fn prepare(&mut self, p: &Prepare) {
        let rate = p.sample_rate();
        if self
            .sample_rate
            .is_some_and(|had| (had.get() - rate.get()).abs() <= 0.1)
        {
            return;
        }
        self.sample_rate = Some(rate);
        self.click_normal = Self::generate_click(rate, false);
        self.click_accent = Self::generate_click(rate, true);
        self.go_silent();
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (_, mut outputs) = io.split();
        let mut channels = outputs.iter_mut();
        let (Some(left), Some(right)) = (channels.next(), channels.next()) else {
            return Status::Modified;
        };
        self.render(cx.env, left, right);
        Status::Modified
    }

    fn reset(&mut self) {
        self.go_silent();
    }
}

/// [`ClickNode`]'s fork source: a template sharing the live node's settings
/// and count-in cell, forked into a node that shares neither.
struct ClickFork(ClickNode);

impl ClickFork {
    /// The fork: the settings and the count-in flag as they stand now, in
    /// cells of its own, and nothing sounding.
    fn node(&self) -> ClickNode {
        let mut fork = self.0.clone();
        fork.settings = Arc::new(self.0.settings.snapshot());
        fork.preroll = Arc::new(AtomicBool::new(self.0.preroll.load(Ordering::Acquire)));
        fork.go_silent();
        fork
    }
}

impl ForkSource for ClickFork {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(self.node())))
    }
}

/// Inserted with its [`ClickSettings`] as its controls and a fork that
/// renders the metronome as it was set at the fork (see [`ClickNode`]).
impl IntoNode for ClickNode {
    type Controls = Arc<ClickSettings>;

    fn into_parts(self) -> NodeParts<Arc<ClickSettings>> {
        let controls = Arc::clone(&self.settings);
        let fork = ClickFork(self.clone());
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Bpm, FrameClock};
    use tutti_graph::contract::{drive_in, prepared};
    use tutti_graph::{Offset, TransportChanges};
    use tutti_types::meter::{BeatsPerBar, MeterChange, NoteValue, TimeSignature};
    use tutti_types::Frame;

    const SR: f64 = 44_100.0;

    /// The session state a block is rendered under: playing and recording.
    #[derive(Clone, Copy)]
    struct Deck {
        playing: bool,
        recording: bool,
    }

    const ROLLING: Deck = Deck {
        playing: true,
        recording: false,
    };
    const STOPPED: Deck = Deck {
        playing: false,
        recording: false,
    };

    /// A transport at `beat` that holds it for the whole block (a tempo of
    /// zero), under `deck`: the constant beat a test positions the playhead
    /// at.
    fn held(deck: Deck, beat: f64) -> tutti_graph::Transport {
        tutti_graph::Transport::new(deck.playing, Bpm(0.0), Beat(beat), None)
            .with_recording(deck.recording)
    }

    /// A block of `len` frames at `rate` under `transport`, no changes.
    fn env(rate: f64, len: usize, transport: tutti_graph::Transport) -> Env {
        Env {
            frame: Frame(0),
            sample_rate: crate::SampleRate(rate),
            block_len: Samples(len),
            transport,
            changes: TransportChanges::NONE,
        }
    }

    /// One block, as `[left, right]`.
    fn block(node: &mut ClickNode, env: &Env) -> [Vec<f32>; 2] {
        let out = drive_in(node, env, &[], &[], &[]).audio;
        let [l, r]: [Vec<f32>; 2] = out.try_into().expect("stereo");
        [l, r]
    }

    /// One frame at `beat`, as a `[left, right]` pair.
    fn frame(node: &mut ClickNode, deck: Deck, beat: f64) -> [f32; 2] {
        let [l, r] = block(node, &env(SR, 1, held(deck, beat)));
        [l[0], r[0]]
    }

    fn make_click() -> (Transport, Arc<ClickSettings>, ClickNode) {
        let transport = Transport::new(SR);
        let settings = Arc::new(ClickSettings::new());
        let node = prepared(
            ClickNode::new(&transport, Arc::clone(&settings)),
            crate::SampleRate(SR),
            64,
        );
        (transport, settings, node)
    }

    /// Whether any of `n` one-frame blocks at `beat` sounds.
    fn sounds_within(node: &mut ClickNode, deck: Deck, beat: f64, n: usize) -> bool {
        (0..n).any(|_| {
            let out = frame(node, deck, beat);
            out[0] != 0.0 || out[1] != 0.0
        })
    }

    #[test]
    fn test_click_node_silent_when_paused() {
        let (_, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::Always);

        let output = frame(&mut node, STOPPED, 0.0);
        assert_eq!(output[0], 0.0);
        assert_eq!(output[1], 0.0);
    }

    #[test]
    fn test_click_node_plays_on_beat() {
        let (_, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);

        assert!(
            sounds_within(&mut node, ROLLING, 0.0, 100),
            "Click should produce non-zero output"
        );
        assert!(
            sounds_within(&mut node, ROLLING, 1.0, 100),
            "Click should produce non-zero output on new beat"
        );
    }

    #[test]
    fn test_click_plays_after_loop_wrap() {
        let (_, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);

        let _ = frame(&mut node, ROLLING, 7.0);
        assert_eq!(node.last_click_onset, Some(Beat(7.0)));

        // Simulate loop wrap: beat jumps backward from 7 to 4
        let found_nonzero = (0..100).any(|_| frame(&mut node, ROLLING, 4.0)[0] != 0.0);
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
        let (_, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::Always);

        let accents_at = |node: &mut ClickNode, beat: f64| {
            Node::reset(node);
            let _ = frame(node, ROLLING, beat);
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
        let (_, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::Always);
        settings.set_meter(Arc::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH),
        )])));

        // Half a quarter note apart is a full notated beat in 7/8, so these are
        // distinct clicks.
        let _ = frame(&mut node, ROLLING, 0.0);
        assert_eq!(node.last_click_onset, Some(Beat(0.0)));
        assert!(node.is_accent, "beat 0 is the downbeat of bar 1");

        let _ = frame(&mut node, ROLLING, 0.5);
        assert_eq!(
            node.last_click_onset,
            Some(Beat(0.5)),
            "an eighth is one notated beat"
        );
        assert!(!node.is_accent, "beat 2 of the bar is not accented");

        // The next bar starts at 3.5 quarters, not 7.
        let _ = frame(&mut node, ROLLING, 3.5);
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
        let (_, settings, mut node) = make_click();
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
            let _ = frame(&mut node, ROLLING, beat);
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

    /// A transport rolling from `start` so that frame `i` of the block is at
    /// beat `start + i/100` (at 44.1 kHz): 26 460 BPM.
    fn hundredths_from(start: f64) -> tutti_graph::Transport {
        tutti_graph::Transport::new(true, Bpm(26_460.0), Beat(start), None)
    }

    /// A meter change that lands *inside* a notated beat still clicks its
    /// downbeat, on its frame.
    ///
    /// Within a block the onset check skips `bar_at` while the playhead stays in
    /// the latched beat's span, so that span must end at the change rather than
    /// at the next onset of the old meter — a change always begins a bar, and
    /// here it begins one half-way through a quarter note. Driven as one block
    /// with the change mid-block, because the span is dropped at every block
    /// boundary: a frame-by-frame version of this test passes with or without
    /// the cap.
    ///
    /// Mutation: ending the latched span at `onset + beat_length` alone, without
    /// the cap at the next change, keeps beat 2.5 inside `[2.0, 3.0)` for the
    /// rest of the block, so the downbeat does not click and every assertion
    /// fails.
    #[test]
    fn a_mid_beat_meter_change_clicks_its_downbeat() {
        let (_, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);
        settings.set_meter(Arc::new(MeterMap::new([
            MeterChange::new(Beat(0.0), TimeSignature::default()),
            MeterChange::new(
                Beat(2.5),
                TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH),
            ),
        ])));

        // Beat 2's click has already played out, so only the change can sound.
        node.last_click_onset = Some(Beat(2.0));
        node.click_pos = node.click_normal.len();

        // Frame `i` carries beat `2 + i/100`, so the change at 2.5 is frame 50.
        let [left, _] = block(&mut node, &env(SR, 64, hundredths_from(2.0)));

        assert_eq!(
            node.last_click_onset,
            Some(Beat(2.5)),
            "the change's downbeat is a new onset"
        );
        assert!(node.is_accent, "and it is a downbeat");
        let first_sound = left.iter().position(|&s| s != 0.0);
        // The click's first frame is exactly zero, so it sounds one frame after
        // its onset — see `click_onset_is_sample_accurate_within_the_block`.
        assert_eq!(first_sound, Some(51), "clicked on the change's frame");
    }

    /// A meter published while a beat is latched is in force from the next
    /// block, not from whenever the playhead leaves the span the old meter drew.
    ///
    /// Mutation: removing `forget_latch` from `render` keeps the 4/4 span
    /// `[2.0, 3.0)` across the publish, so the node still holds onset 2.0 after
    /// the second block and fails.
    #[test]
    fn a_republished_meter_is_seen_within_one_block() {
        let (_, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::Always);

        // 4/4 (the default): 2.00..2.10 all belong to the quarter at 2.0.
        let _ = block(&mut node, &env(SR, 10, hundredths_from(2.0)));
        assert_eq!(node.last_click_onset, Some(Beat(2.0)));

        // 3/16: sixteenth-note beats, three to a 0.75-quarter bar. Beat 2.64
        // falls in the sixteenth starting at 2.5 — inside the old span.
        settings.set_meter(Arc::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            TimeSignature::new(BeatsPerBar::new(3), NoteValue::SIXTEENTH),
        )])));
        let _ = block(&mut node, &env(SR, 10, hundredths_from(2.64)));
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
        let (_, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::Always);

        // One bar before the start: a downbeat, and no panic or wrap.
        let _ = frame(&mut node, ROLLING, -4.0);
        assert!(node.is_accent, "-4.0 in 4/4 is the downbeat of bar 0");

        let _ = frame(&mut node, ROLLING, -3.0);
        assert!(!node.is_accent, "-3.0 is beat 2 of bar 0");
    }

    /// A block renders exactly what the same frames render cut into
    /// one-frame blocks: the per-block reads (mode, volume, meter) change
    /// nothing while they are stable, and the beat is per frame.
    ///
    /// The beat *moves* through the block and crosses an onset mid-block, so
    /// this pins the per-frame onset path as well as the steady one: a node
    /// that read the beat once per block would start the click on frame 0 and
    /// diverge from the one-frame blocks there.
    ///
    /// Mutation (run): `piece_beats` emitting the piece's first beat on every
    /// frame (`emit(i, t.beat())`) → the whole block holds beat 2 − 20/22 050,
    /// no onset in the block → the not-silence assert fails.
    #[test]
    fn a_block_matches_its_frames_one_by_one() {
        let (_, settings, mut block_node) = make_click();
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);

        // 44.1 kHz at 120 BPM is 1/22050 beat per frame, so starting 20 frames
        // before beat 2 puts that onset at frame 20 of the block.
        const N: usize = 64;
        let mut host = FrameClock::new(
            Beat(2.0 - 20.0 / 22_050.0),
            Bpm(120.0),
            crate::SampleRate(SR),
        );
        let counted = |host: &FrameClock| {
            tutti_graph::Transport::counted(true, host.tempo(), host.origin(), None)
        };

        // Latch the beat before it, as a transport rolling into this block
        // would have, with its click already played out, so the onset at beat 2
        // is the only sound in the block.
        block_node.last_click_onset = Some(Beat(1.0));
        block_node.click_pos = block_node.click_normal.len();
        let mut frame_node = block_node.clone();

        let [left, right] = block(&mut block_node, &env(SR, N, counted(&host)));
        assert!(
            left.iter().any(|&s| s != 0.0),
            "the onset must fall inside the block, or this compares silence"
        );

        for i in 0..N {
            let [l, r] = block(&mut frame_node, &env(SR, 1, counted(&host)));
            host.advance(Samples(1), None);
            assert_eq!(left[i], l[0], "left channel diverged at frame {i}");
            assert_eq!(right[i], r[0], "right channel diverged at frame {i}");
        }
    }

    /// A click starts on the frame where the playhead reaches its onset — not
    /// on the next block boundary — at more than one block size.
    ///
    /// A node that read one beat per block from the clock's writeback would
    /// start every click on a block boundary, up to a block late.
    ///
    /// The expected frame is derived from tempo arithmetic alone, not from the
    /// node: the host's clock is at `start + i·bps` on frame `i` (emit, then
    /// advance), so the first frame at or past the onset is
    /// `ceil((onset − start) / bps)`. `±1` absorbs rounding across that
    /// boundary. Each block's transport is the host clock's, counted, as the
    /// engine hands it.
    ///
    /// Mutation: reading the beat of frame 0 of the block for the whole block
    /// (a once-per-block read) moves the onset to the next block
    /// boundary and fails at both block sizes: frame 2112 at 64 and 2120 at
    /// 40, against 2103. (A second size that shared a boundary with the first
    /// would prove nothing more, so 40 is chosen to land elsewhere.)
    #[test]
    fn click_onset_is_sample_accurate_within_the_block() {
        const RATE: f64 = 48_000.0;
        const BPM: f64 = 137.0;
        const START: f64 = 0.9;
        const ONSET: f64 = 1.0;

        for len in [64usize, 40] {
            let transport = Transport::new(RATE);
            let settings = Arc::new(ClickSettings::new());
            settings.set_mode(MetronomeMode::Always);
            settings.set_volume(1.0);
            let mut node = prepared(
                ClickNode::new(&transport, settings),
                crate::SampleRate(RATE),
                len,
            );
            // The click's first frame is exactly zero (`sin(0)` under a zero
            // attack) and its second is not, so the onset is one before the
            // first non-zero frame. Asserted, since the test leans on it.
            assert_eq!(node.click_normal[0], 0.0);
            assert_ne!(node.click_normal[1], 0.0);
            // As if the transport had rolled through beat 0 already: the click
            // under test is the one at beat 1, and beat 0's has played out.
            node.last_click_onset = Some(Beat(0.0));
            node.click_pos = node.click_normal.len();

            let bps = super::super::beats_per_sample(BPM, RATE).get();
            let expected = ((ONSET - START) / bps).ceil() as usize;
            assert_ne!(
                expected % len,
                0,
                "the onset must land mid-block, or a block-quantised click passes"
            );

            let mut host = FrameClock::new(Beat(START), Bpm(BPM), crate::SampleRate(RATE));
            let mut left = Vec::new();
            while left.len() < expected + 2 * len {
                let t = tutti_graph::Transport::counted(true, host.tempo(), host.origin(), None);
                let [l, _] = block(&mut node, &env(RATE, len, t));
                host.advance(Samples(len), None);
                left.extend(l);
            }

            let first_sound = left
                .iter()
                .position(|s| *s != 0.0)
                .expect("the click at beat 1 must sound");
            let onset = first_sound - 1;
            assert!(
                onset.abs_diff(expected) <= 1,
                "block {len}: click started at frame {onset}, the beat reaches \
                 {ONSET} at frame {expected}"
            );
        }
    }

    /// A stop inside a block silences the click from its frame, and a start
    /// inside one sounds the beat it lands on from its frame: the play state
    /// is the block's pieces', not the block start's.
    ///
    /// Before its port to `tutti_graph::Node` the click read the live play flag once per
    /// 64-frame chunk, so a mid-block start or stop gated it from the
    /// block's start (the gap `tests/env_beat.rs` recorded).
    ///
    /// Mutations (run): gate the whole block on `env.transport` (the first
    /// piece's) → the stop at 30 does not silence frames 30.. and the start
    /// at 10 is not heard → fails.
    #[test]
    fn a_start_or_stop_inside_a_block_gates_on_its_frame() {
        let (_, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);

        // Rolling on beat 3 (its click sounding), stopped at frame 30.
        let mut changes = TransportChanges::NONE;
        changes
            .push(
                Offset::new(30, Samples(64)).expect("inside"),
                held(STOPPED, 3.0),
            )
            .expect("fits");
        let mut rolling = env(SR, 64, held(ROLLING, 3.0));
        rolling.changes = changes;
        let [left, _] = block(&mut node, &rolling);
        assert!(left[1..30].iter().all(|&s| s != 0.0), "sounding until 30");
        assert!(left[30..].iter().all(|&s| s == 0.0), "silent from 30");

        // Stopped, then started at frame 10 on beat 5.
        let mut changes = TransportChanges::NONE;
        changes
            .push(
                Offset::new(10, Samples(64)).expect("inside"),
                held(ROLLING, 5.0),
            )
            .expect("fits");
        let mut starting = env(SR, 64, held(STOPPED, 4.0));
        starting.changes = changes;
        let [left, _] = block(&mut node, &starting);
        assert!(left[..11].iter().all(|&s| s == 0.0), "silent until 10");
        assert_ne!(left[11], 0.0, "beat 5's click from frame 10");
    }

    /// Eight blocks of the metronome, hashed bit-for-bit against a reference
    /// render.
    ///
    /// # What this pins, and why a hash
    ///
    /// The render must stay an *identity*: the same samples, not merely
    /// samples that still pass the behavioural tests above. Those tests check
    /// onsets, accents and mode gating — every one of them would pass a click
    /// whose envelope had drifted by an LSB, and none of them would name it.
    ///
    /// The reference figure is FNV-1a over the raw `f32` bits of all 1,024
    /// samples of this exact schedule, `0xE011_43CA_E6D4_ECD5`. A hash rather
    /// than a 1,024-entry array because the array would be unreadable and
    /// unmaintained, while any single-bit difference moves it.
    ///
    /// The schedule holds the beat constant across each 64-frame block (a
    /// rolling transport at a tempo of zero, stepped an eighth note per
    /// block), so a per-block and a per-frame beat read give the same samples.
    ///
    /// # Mutation-tested
    ///
    /// Verified to fail, not merely to pass: scaling one sample by `1.0 + f32::EPSILON`
    /// changes the digest. It also fails if `generate_click`'s envelope
    /// changes, or if the beat schedule below is edited — all of which is the
    /// point. Recompute the constant ONLY with a deliberate, argued DSP change.
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
        let settings = Arc::new(ClickSettings::new());
        settings.set_mode(MetronomeMode::Always);
        settings.set_volume(1.0);
        let mut node = prepared(
            ClickNode::new(&transport, Arc::clone(&settings)),
            crate::SampleRate(48_000.0),
            64,
        );

        let mut rendered = Vec::with_capacity(8 * 64 * 2);
        for b in 0..8 {
            let [l, r] = block(
                &mut node,
                &env(48_000.0, 64, held(ROLLING, f64::from(b) * 0.5)),
            );
            for i in 0..64 {
                rendered.push(l[i]);
                rendered.push(r[i]);
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

    #[test]
    fn test_preroll_only_mode() {
        let (transport, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::PrerollOnly);
        settings.set_volume(1.0);

        // Should be silent when not in preroll
        let output = frame(&mut node, ROLLING, 0.0);
        assert_eq!(output[0], 0.0);

        // Enable preroll - should play
        transport.settings.set_in_preroll(true);
        Node::reset(&mut node);
        assert!(
            sounds_within(&mut node, ROLLING, 0.0, 100),
            "Click should play during preroll"
        );
    }

    #[test]
    fn test_recording_only_mode() {
        let (transport, settings, mut node) = make_click();
        settings.set_mode(MetronomeMode::RecordingOnly);
        settings.set_volume(1.0);
        let recording = Deck {
            playing: true,
            recording: true,
        };

        // Should be silent when not recording
        let output = frame(&mut node, ROLLING, 0.0);
        assert_eq!(output[0], 0.0);

        // Enable recording - should play
        Node::reset(&mut node);
        assert!(
            sounds_within(&mut node, recording, 0.0, 100),
            "Click should play during recording"
        );

        // In preroll while recording - should NOT play
        transport.settings.set_in_preroll(true);
        Node::reset(&mut node);
        let output = frame(&mut node, recording, 0.0);
        assert_eq!(
            output[0], 0.0,
            "Click should not play during preroll in RecordingOnly mode"
        );
    }

    /// A fork of the click renders the metronome as it was set when it was
    /// taken: volume, mode, meter and the count-in flag come from cells of
    /// its own, so none of the four live moves below reaches it — and each
    /// is heard by a fork taken after it. The play state is the fork's own
    /// block transport, by construction.
    ///
    /// Mutations (run): `ClickFork::node` keeping the live settings `Arc` →
    /// volume, mode and meter fail; keeping the live preroll `Arc` → the
    /// count-in fails; building the fresh settings with `ClickSettings::new()`'s
    /// defaults instead of `snapshot` → the fork is silent (mode Off), so
    /// every control fails as inaudible.
    #[test]
    fn a_fork_snapshots_settings_and_the_count_in() {
        type Move = fn(&Transport, &ClickSettings);
        let moves: [(&str, Move); 4] = [
            ("volume", |_, s| s.set_volume(1.0)),
            ("mode", |_, s| s.set_mode(MetronomeMode::Off)),
            ("meter", |_, s| {
                s.set_meter(Arc::new(MeterMap::new([MeterChange::new(
                    Beat(0.0),
                    TimeSignature::new(BeatsPerBar::new(3), NoteValue::QUARTER),
                )])))
            }),
            ("count-in", |t, _| t.settings.set_in_preroll(true)),
        ];
        let fresh = || {
            let transport = Transport::new(48_000.0);
            let settings = Arc::new(ClickSettings::new());
            settings.set_volume(0.5);
            // Recording-only, so the count-in gates it (the render records).
            settings.set_mode(MetronomeMode::RecordingOnly);
            let node = ClickNode::new(&transport, Arc::clone(&settings));
            (transport, settings, ClickFork(node))
        };
        // 10 240 frames rolling (and recording) from beat 0 at 20 beats a
        // second: four clicks, past beat 3 (a downbeat in 3/4, not in 4/4).
        let render = |node: ClickNode| {
            let mut node = prepared(node, crate::SampleRate(48_000.0), 64);
            let mut host = FrameClock::new(Beat(0.0), Bpm(1_200.0), crate::SampleRate(48_000.0));
            let mut left = Vec::new();
            for _ in 0..160 {
                let t = tutti_graph::Transport::counted(true, host.tempo(), host.origin(), None)
                    .with_recording(true);
                left.extend(block(&mut node, &env(48_000.0, 64, t))[0].clone());
                host.advance(Samples(64), None);
            }
            left
        };
        let (_, _, source) = fresh();
        let baseline = render(source.node());
        assert!(baseline.iter().any(|&s| s != 0.0), "the fork sounds");
        for (what, apply) in moves {
            let (transport, settings, source) = fresh();
            let taken = source.node();
            apply(&transport, &settings);
            assert_eq!(
                render(taken),
                baseline,
                "{what}: a live move reached a fork taken before it"
            );
            assert_ne!(
                render(source.node()),
                baseline,
                "{what}: a fork taken after the move does not hear it"
            );
        }
    }
}
