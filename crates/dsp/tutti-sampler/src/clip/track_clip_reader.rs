//! Per-track clip reader: a single graph node that internally manages
//! all audio clip playback for one track.
//!
//! Replaces the old "one `SamplerUnit` graph node per clip + dynamic
//! `StereoSumUnit`" model. ECS systems send [`ClipCommand`]s through
//! a [`TrackClipReaderHandle`]; the unit drains them each audio buffer.
//!
//! Each clip is played by a [`Voice`]: a [`VoiceSource`] (EITHER an in-memory
//! [`SamplerUnit`] with the whole clip resident in RAM as an `Arc<Wave>`, decoded
//! once by the wave cache, OR a [`StreamingClipReader`] that pulls incrementally
//! from the butler ring) plus its [`Playback`] control-intent record. The source
//! choice is a monomorphized [`VoiceSource`] enum, not a boxed trait object, so
//! the per-buffer match stays inlinable and the hot path allocation-free. The
//! optional time-stretch processor wraps whichever source when a clip is
//! stretched/pitched (both variants `impl AudioUnit`). A single [`Voice`] can
//! also stand on its own as a [`VoiceNode`] graph node (resynth / preview),
//! sharing the exact per-voice read the mixer does per slot.

use std::sync::Arc;

use crate::ports::{Command, Commands};
use crate::stretch;
use crate::MAX_SAMPLER_CHANNELS;

use super::sampler_unit::{LoopSetting, SamplerUnit, TransportPlacement};
use super::streaming_sampler::StreamingClipReader;
#[cfg(feature = "bevy")]
use bevy_ecs::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use tutti_core::{
    AudioUnit, Beat, BeatDuration, BufferMut, BufferRef, Cents, Linear, PlaybackRate,
    SamplePosition, SignalFrame, StretchFactor, Timeline, Wave,
};

const COMMAND_CAPACITY: usize = 64;
const TRACK_CLIP_READER_ID: u64 = 0x_0000_0000_0000_DA03;

// ---------------------------------------------------------------------------
// Slot ID — opaque u128 so bevy-tutti stays independent of dawai-types.
// dawai-model converts ClipId ↔ SlotId at the boundary.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(pub u128);

// ---------------------------------------------------------------------------
// Direction — playback direction for a clip. Replaces loose `reverse: bool`
// so the intent reads at every call site.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    #[default]
    Forward,
    Reverse,
}

impl Direction {
    #[inline]
    pub fn is_reverse(self) -> bool {
        matches!(self, Self::Reverse)
    }

    /// `true` → `Reverse`, `false` → `Forward`.
    #[inline]
    pub fn from_reverse(reverse: bool) -> Self {
        if reverse {
            Self::Reverse
        } else {
            Self::Forward
        }
    }
}

// ---------------------------------------------------------------------------
// VoiceSource — a voice's audio source, monomorphized. Either the whole clip is
// resident in RAM (`Ram`) or it streams incrementally from the butler ring
// (`Disk`).
//
// DESIGN INVARIANT: the two variants differ ONLY in the *essential* per-sample
// read — `Ram` indexes an `Arc<Wave>`; `Disk` pops the butler-fed ring,
// emitting silence while `is_seeking()` and crossfading on refill. Every *cold*
// control op is a `match` on this enum — see `apply_gain` / `apply_speed` /
// `apply_direction` / `apply_placement`. A `ClipReader` trait used to sit over
// the pair, but half its methods no-opped on one side or the other, which hid
// the divergence instead of removing it (streaming clamped speed, in-RAM did
// not; `set_wave` silently did nothing on disk). With two in-crate impls the
// enum is the better tool: it inlines, it surfaces the fork at the call site,
// and adding a variant makes the compiler list every decision to make.
//
// The enum (not a `Box<dyn AudioUnit>`) is also what RT needs: monomorphized
// dispatch on `tick`/`process`, so the per-sample read never touches a vtable
// or the heap.
// Both variants are `Clone` and `impl AudioUnit`, so the field-wise `Voice`
// clone and the stretch wrapper work uniformly across them.
// ---------------------------------------------------------------------------

#[non_exhaustive]
pub enum VoiceSource {
    Ram(SamplerUnit),
    Disk(StreamingClipReader),
}

// Hand-rolled: both variants wrap non-`Debug`-deriving units (`SamplerUnit` /
// `StreamingClipReader`), each with its own hand-rolled summary Debug.
impl std::fmt::Debug for VoiceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ram(s) => f.debug_tuple("Ram").field(s).finish(),
            Self::Disk(s) => f.debug_tuple("Disk").field(s).finish(),
        }
    }
}

impl Clone for VoiceSource {
    fn clone(&self) -> Self {
        match self {
            Self::Ram(s) => Self::Ram(s.clone()),
            Self::Disk(s) => Self::Disk(s.clone()),
        }
    }
}

impl VoiceSource {
    /// The source as an [`AudioUnit`], for the verbs every node has
    /// (`reset`, `set_sample_rate`). Tier-specific control is a `match` at the
    /// call site instead — see [`apply_gain`](Self::apply_gain).
    #[inline]
    fn as_audio_unit_mut(&mut self) -> &mut dyn AudioUnit {
        match self {
            Self::Ram(s) => s,
            Self::Disk(s) => s,
        }
    }

    /// Set the output gain. Both tiers store a linear multiplier applied after
    /// the source read, so this is genuinely one operation.
    #[inline]
    fn apply_gain(&mut self, gain: Linear) {
        match self {
            Self::Ram(s) => s.set_gain(gain),
            Self::Disk(s) => s.set_gain(gain),
        }
    }

    /// Set varispeed.
    ///
    /// The two tiers store it differently — a unit-local field in RAM, an
    /// atomic shared with the butler and every clone on disk — which is exactly
    /// why the bound now lives in [`PlaybackRate`] rather than in one of these
    /// arms.
    #[inline]
    fn apply_speed(&mut self, speed: PlaybackRate) {
        match self {
            Self::Ram(s) => s.set_speed(speed),
            Self::Disk(s) => s.set_speed(speed),
        }
    }

    /// Set playback direction.
    ///
    /// In-RAM direction lives on the slot's `Playback`, not inside the unit —
    /// the reversed index is applied at read time — so only the streaming tier
    /// has source-side state to update. The caller writes `play.direction`
    /// either way.
    #[inline]
    fn apply_direction(&mut self, direction: Direction) {
        match self {
            Self::Ram(_) => {}
            Self::Disk(s) => s.set_direction(direction),
        }
    }

    /// Update the timeline placement window.
    #[inline]
    fn apply_placement(&mut self, start_beat: Beat, duration: Option<BeatDuration>) {
        match self {
            Self::Ram(s) => s.set_placement(start_beat, duration),
            Self::Disk(s) => s.set_placement(start_beat, duration),
        }
    }
}

// ---------------------------------------------------------------------------
// Playback — the control-INTENT record for one voice. It says *what* the voice
// should do (gain, speed, direction, loop, timeline placement, stretch, pitch);
// each [`VoiceSource`] APPLIES it its own way (the in-RAM `SamplerUnit` stores
// the state on its resident DSP; the streaming reader forwards to the butler's
// shared `RtState`). The apply fan-out is the `VoiceSource` match — Playback is
// the description, not the applied state.
//
// `Default` is hand-written: the newtypes default to zero, so a derived default
// would ship silent (`gain = 0`) and frozen (`speed = 0` / `stretch = 0`).
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Playback {
    pub gain: Linear,
    pub speed: PlaybackRate,
    pub direction: Direction,
    pub loop_: LoopSetting,
    pub placement: Option<TransportPlacement>,
    /// Time-stretch factor (1.0 = no stretch). Absorbed here so the placement,
    /// stretch, and pitch intent live in one record — `ClipSpec` no longer keeps
    /// a stretch sidecar.
    pub stretch: StretchFactor,
    /// Pitch shift in cents (0.0 = no shift).
    pub pitch: Cents,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            gain: Linear::new(1.0),
            speed: PlaybackRate::UNITY,
            direction: Direction::Forward,
            loop_: LoopSetting::Off,
            placement: None,
            stretch: StretchFactor::UNITY,
            pitch: Cents::new(0.0),
        }
    }
}

/// The clip's control fields as decoded from ECS at promote time — the flat
/// input to [`Playback::from_pending`]. A clip carries a single `gain` scalar
/// that dawai maps to BOTH the voice gain and the read speed (the historical
/// `playback_rate`), plus the loop range, reverse, and stretch/pitch intent.
/// Grouped so both the RAM promote and the Disk poll build a `Playback` from
/// one shape.
#[derive(Debug, Clone, Default)]
pub struct PendingPlayback {
    pub gain: Linear,
    pub speed: PlaybackRate,
    pub direction: Direction,
    pub looping: bool,
    pub loop_start: SamplePosition,
    pub loop_end: SamplePosition,
    pub stretch: StretchFactor,
    pub pitch: Cents,
    pub placement: Option<TransportPlacement>,
}

impl Playback {
    /// Build the control-intent record for a freshly-promoted clip from its
    /// decoded control fields. This is the single place the pending clip's
    /// gain / loop / reverse / stretch DATA becomes a [`Playback`] — replacing
    /// the old pre-send poking of the `SamplerUnit` (`set_gain`, the
    /// `set_loop_range` / `set_looping` ladder) on the dawai side. The reader's
    /// `insert_voice` then applies this record per-tier.
    ///
    /// Loop mapping matches the already-migrated `UpdateLoop` update path: an
    /// explicit `[start, end)` range with `end > start` primes a 256-sample
    /// crossfade; loop-enabled with no valid range primes a zero range
    /// (`On { 0, 0, 0 }`), exactly as the update path sends it; disabled →
    /// `Off`.
    pub fn from_pending(p: PendingPlayback) -> Self {
        let loop_ = if p.looping {
            if p.loop_end.get() > p.loop_start.get() {
                LoopSetting::On {
                    start: p.loop_start,
                    end: p.loop_end,
                    crossfade_samples: 256,
                }
            } else {
                LoopSetting::On {
                    start: SamplePosition::new(0.0),
                    end: SamplePosition::new(0.0),
                    crossfade_samples: 0,
                }
            }
        } else {
            LoopSetting::Off
        };
        Self {
            gain: p.gain,
            speed: p.speed,
            direction: p.direction,
            loop_,
            placement: p.placement,
            stretch: p.stretch,
            pitch: p.pitch,
        }
    }
}

impl Clone for Playback {
    fn clone(&self) -> Self {
        Self {
            gain: self.gain,
            speed: self.speed,
            direction: self.direction,
            loop_: self.loop_.clone(),
            placement: self.placement.clone(),
            stretch: self.stretch,
            pitch: self.pitch,
        }
    }
}

// ---------------------------------------------------------------------------
// Voice — one clip's playback state, and what the reader STORES per slot. Its
// `source` is either an in-memory `SamplerUnit` or a streaming
// `StreamingClipReader` (the [`VoiceSource`] enum); `play` is the [`Playback`]
// control-intent record; `channel_index` is the butler channel for a `Disk`
// source (`None` for `Ram`), kept on the Voice because the butler loop routing
// needs it.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Voice {
    pub source: VoiceSource,
    pub play: Playback,
    /// Butler channel index for a `Disk` source; `None` for `Ram`. The reader
    /// drain forwards streaming loop ops (`SetStreamLoop` / `ClearStreamLoop`)
    /// to this channel via the typed [`Commands`] handle — loop is butler-owned
    /// (it reads a fadein head off disk + mutates `plan.link.loop_config`,
    /// neither reachable from the reader), so the forward is the honest path.
    /// Meaningless for `Ram` (loop is primed directly on the `SamplerUnit`).
    pub channel_index: Option<usize>,
}

impl Clone for Voice {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            play: self.play.clone(),
            channel_index: self.channel_index,
        }
    }
}

impl Voice {
    /// Replace the transport clock behind the voice, preserving the existing
    /// start-beat / duration. Used by the offline region render to rebind a
    /// standalone voice onto the export transport.
    ///
    /// Rebinds BOTH clocks that must move together:
    /// - `play.placement.transport` — the control-intent record.
    /// - the source's own read clock. The `Ram` [`SamplerUnit`] reads its
    ///   sample position from its OWN placement (`transport_sample_position`),
    ///   NOT from `play.placement`, so rebinding only the intent record would
    ///   leave the actual read clock on the live transport — the offline render
    ///   would then read the wrong (undriven) playhead and render silence. The
    ///   `Disk` streaming source owns its transport internally likewise; both are
    ///   covered here so the offline rebind is complete.
    pub fn replace_transport(&mut self, transport: Arc<dyn Timeline>) {
        if let Some(placement) = &mut self.play.placement {
            placement.transport = transport.clone();
        }
        // Rebind the source's own read clock. Only the `Ram` `SamplerUnit`
        // exposes a whole-transport swap (`replace_transport`); it is the only
        // source a standalone offline `VoiceNode` ever wraps (resynth /
        // region-render populate build RAM voices), so this is the path that
        // matters for the offline render.
        if let VoiceSource::Ram(sampler) = &mut self.source {
            sampler.replace_transport(transport);
        }
    }
}

// ---------------------------------------------------------------------------
// ClipSlot — a `Voice` plus the resident time-stretch DSP processor. The
// processor is the DSP object (not config), so it stays on the slot alongside
// the engine sample rate; the control intent lives in `voice.play`.
// ---------------------------------------------------------------------------

struct ClipSlot {
    id: SlotId,
    voice: Voice,
    /// The time-stretch processor is **always resident**: it is built once (two
    /// phase-vocoder constructions + four `RtScratch` scratch buffers) when the
    /// slot is created, off the per-buffer hot path. The audio thread never
    /// (re)builds it — it only flips the lock-free `stretch_factor` /
    /// `pitch_cents` atomics inside it. It owns NO copy of the clip source: it is
    /// a pure frame-in → frame-out filter. At tick time, the `needs_stretch()`
    /// gate (mirrored from those atomics into `voice.play.stretch` /
    /// `voice.play.pitch`) chooses whether to tick the single source and route
    /// its frame through this filter, or read the source directly. The heavy
    /// construction stays off the audio thread this way:
    /// [`ClipCommand::UpdateStretch`] only sets atomics, never allocates.
    ///
    /// Structural invariant: `voice.play.stretch` / `voice.play.pitch` cannot
    /// drift from the processor's atomics — every mutation goes through
    /// [`ClipSlot::set_stretch`], which writes both in one step.
    stretch: stretch::Unit,
    sample_rate: f64,
}

impl ClipSlot {
    /// Build a slot with the resident stretch unit already materialised and its
    /// atomics primed from `voice.play.stretch` / `voice.play.pitch` — the single
    /// constructor both the live `AddVoice` path and the synchronous
    /// `insert_clip` path go through. The heavy `stretch::Unit` construction
    /// happens here, at slot-creation time, never on the per-buffer command
    /// drain.
    /// Width is explicit at every call site — there is no stereo-defaulting
    /// `new`, because both callers (the reader's drain and `VoiceNode`) know
    /// their own width and a default here would silently mismatch it.
    ///
    /// The stretch unit is built at the **slot's** width: a narrower stretcher
    /// would truncate the frames fed to it, and only on the stretch-enabled
    /// branch — a failure mode no stereo test can see.
    fn with_channels(id: SlotId, voice: Voice, sample_rate: f64, channels: usize) -> Self {
        let stretch = stretch::Unit::with_channels(sample_rate, channels);
        stretch.set_stretch_factor(voice.play.stretch);
        stretch.set_pitch_cents(voice.play.pitch);
        Self {
            id,
            voice,
            stretch,
            sample_rate,
        }
    }

    fn needs_stretch(&self) -> bool {
        (self.voice.play.stretch.get() - 1.0).abs() > 0.001
            || self.voice.play.pitch.get().abs() > 0.5
    }

    /// Update the stretch factors — the only entry point for mutating them.
    /// Lock-free: flips the resident processor's atomics and mirrors the values
    /// into `voice.play` (read by the `needs_stretch()` routing gate).
    /// Allocation-free, so it is safe to run on the audio-thread command drain.
    fn set_stretch(&mut self, stretch_factor: StretchFactor, pitch_cents: Cents) {
        self.voice.play.stretch = stretch_factor;
        self.voice.play.pitch = pitch_cents;
        self.stretch.set_stretch_factor(stretch_factor);
        self.stretch.set_pitch_cents(pitch_cents);
    }

    /// Read ONE mixed stereo frame from this slot: the exact per-variant read the
    /// `tick` mixdown does for a single slot, routed through the resident stretch
    /// filter when `needs_stretch()`. Factored so the mixer loop and the
    /// standalone [`VoiceNode`] share one definition — no duplication, no dyn.
    /// Alloc-free: returns a stack frame. RT: the `VoiceSource` enum match is
    /// unchanged, only relocated here.
    #[inline]
    fn tick_frame_into(&mut self, out: &mut [f32]) {
        let direction = self.voice.play.direction;
        let gain = self.voice.play.gain;
        if self.needs_stretch() {
            // Tick the SINGLE source once to get its raw frame (the same
            // per-variant read the else-branch uses — the `VoiceSource` enum
            // still owns the read), then feed that frame into the stretch filter.
            // This is the one place a scratch frame is genuinely unavoidable:
            // the filter's input and output cannot be the same slice. Stack
            // array at the fixed ceiling, used as a prefix — no heap.
            let n = out.len().min(MAX_SAMPLER_CHANNELS);
            let mut raw = [0.0f32; MAX_SAMPLER_CHANNELS];
            read_source_frame_into(&mut self.voice.source, direction, gain, &mut raw[..n]);
            out.fill(0.0);
            self.stretch.tick(&raw[..n], out);
        } else {
            match &mut self.voice.source {
                VoiceSource::Ram(sampler) => match sampler.transport_sample_position() {
                    Some(pos) => read_clip_sample_into(sampler, direction, pos, gain, out),
                    None => out.fill(0.0),
                },
                VoiceSource::Disk(reader) => {
                    // The `StreamingClipReader` owns its placement gate: it
                    // emits silence outside the clip window and pulls the
                    // butler ring inside it. Alloc-free (preallocated
                    // `fetch_scratch`).
                    out.fill(0.0);
                    reader.tick(&[], out);
                }
            }
        }
    }

    /// Accumulate this slot's block-rate contribution into `output`: the exact
    /// per-variant read the `process` mixdown does for a single slot. Factored so
    /// the mixer loop and the standalone [`VoiceNode`] share one definition. RT:
    /// the `VoiceSource` enum match is unchanged, only relocated here.
    #[inline]
    fn process_into(&mut self, size: usize, channels: usize, output: &mut BufferMut) {
        let direction = self.voice.play.direction;
        let gain = self.voice.play.gain;
        // `BufferMut` is planar with no frame accessor, so a per-sample frame is
        // unavoidable here. Stack array at the fixed ceiling, used as a prefix.
        let n = channels.min(output.channels()).min(MAX_SAMPLER_CHANNELS);
        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];

        /// Accumulate a frame prefix into the planar output at sample `i`.
        macro_rules! mix_in {
            ($frame:expr, $i:expr) => {
                for (c, &s) in $frame.iter().enumerate().take(n) {
                    output.set_f32(c, $i, output.at_f32(c, $i) + s);
                }
            };
        }

        if self.needs_stretch() {
            // Per-sample: read the SINGLE source frame (same per-variant read
            // as the else-branch — the enum still owns the read), then feed
            // it through the stretch filter. The filter's in and out cannot
            // alias, hence the second stack frame.
            let mut raw = [0.0f32; MAX_SAMPLER_CHANNELS];
            match &mut self.voice.source {
                VoiceSource::Ram(sampler) => {
                    let Some(start_pos) = sampler.transport_sample_position() else {
                        return;
                    };
                    let advance = (sampler.speed().get() * sampler.src_ratio().get()) as f64;
                    for i in 0..size {
                        let pos = start_pos + i as f64 * advance;
                        read_clip_sample_into(sampler, direction, pos, gain, &mut raw[..n]);
                        frame[..n].fill(0.0);
                        self.stretch.tick(&raw[..n], &mut frame[..n]);
                        mix_in!(frame, i);
                    }
                }
                VoiceSource::Disk(reader) => {
                    for i in 0..size {
                        raw[..n].fill(0.0);
                        reader.tick(&[], &mut raw[..n]);
                        frame[..n].fill(0.0);
                        self.stretch.tick(&raw[..n], &mut frame[..n]);
                        mix_in!(frame, i);
                    }
                }
            }
        } else {
            match &mut self.voice.source {
                VoiceSource::Ram(sampler) => {
                    let Some(start_pos) = sampler.transport_sample_position() else {
                        return;
                    };
                    let advance = (sampler.speed().get() * sampler.src_ratio().get()) as f64;
                    for i in 0..size {
                        let pos = start_pos + i as f64 * advance;
                        read_clip_sample_into(sampler, direction, pos, gain, &mut frame[..n]);
                        mix_in!(frame, i);
                    }
                }
                VoiceSource::Disk(reader) => {
                    // Sum the reader per-sample (its placement gate + ring
                    // pull run inside each `tick`). Per-sample accumulation
                    // mirrors the stretch branch above and keeps this
                    // alloc-free — no per-slot scratch `BufferMut`.
                    for i in 0..size {
                        frame[..n].fill(0.0);
                        reader.tick(&[], &mut frame[..n]);
                        mix_in!(frame, i);
                    }
                }
            }
        }
    }
}

/// Read the reversed-or-forward clip sample from an in-RAM `SamplerUnit`, scaled
/// by the voice's `gain`. Free function so both the mixer/`VoiceNode` read and
/// the stretch feed share it.
///
/// The `SamplerUnit` is a PURE producer here: it reads the raw interpolated
/// frame via `get_sample_raw` (no gain), and gain is applied ONCE at this
/// Voice/Playback level from `gain` (mirrored from `voice.play.gain`). This
/// matches the streaming tier, where gain lives in the source's shared state,
/// and keeps a single, well-defined gain application point per tier.
#[inline]
fn read_clip_sample_into(
    sampler: &SamplerUnit,
    direction: Direction,
    pos: f64,
    gain: Linear,
    out: &mut [f32],
) {
    match direction {
        Direction::Reverse => {
            let len = sampler.duration_samples() as f64;
            let reversed = (len - 1.0 - pos).max(0.0);
            sampler.get_sample_raw_into(reversed, out);
        }
        Direction::Forward => sampler.get_sample_raw_into(pos, out),
    }
    let g = gain.get();
    for s in out.iter_mut() {
        *s *= g;
    }
}

/// Read ONE frame from a single voice source into `out`, using the exact
/// per-variant read the direct (non-stretch) path uses — the `VoiceSource` enum
/// still owns the read. `gain` scales the in-RAM read at the Voice level (see
/// [`read_clip_sample_into`]); the streaming reader applies its own gain
/// internally. Writes every element of `out`. Used to feed the stretch filter
/// (which owns no source) on the hot path.
#[inline]
fn read_source_frame_into(
    source: &mut VoiceSource,
    direction: Direction,
    gain: Linear,
    out: &mut [f32],
) {
    match source {
        VoiceSource::Ram(sampler) => match sampler.transport_sample_position() {
            Some(pos) => read_clip_sample_into(sampler, direction, pos, gain, out),
            None => out.fill(0.0),
        },
        VoiceSource::Disk(reader) => {
            // `AudioUnit::tick` writes only as many channels as the unit has;
            // clear first so a narrower reader leaves silence, not stale data.
            out.fill(0.0);
            reader.tick(&[], out);
        }
    }
}

// ---------------------------------------------------------------------------
// Commands sent from ECS → audio thread.
// ---------------------------------------------------------------------------

#[non_exhaustive]
pub enum ClipCommand {
    /// Add a clip by handing the reader a fully-formed [`Voice`]: a
    /// [`VoiceSource`] (in-RAM `SamplerUnit` or streaming `StreamingClipReader`)
    /// plus its [`Playback`] control-intent record. The drain builds the
    /// [`ClipSlot`] and applies the full `Playback` per-tier by reusing the same
    /// cold-path appliers the update commands use (`VoiceSource::apply_*` +
    /// `apply_loop`) — no allocation or I/O on the hot path, since the
    /// source (RAM `SamplerUnit` or butler-registered `StreamingClipReader`) is
    /// built entirely on the ECS/butler side before the send.
    ///
    /// This one command replaces the former split `Add` (in-RAM) /
    /// `AddStreaming` (disk) pair: the tier now rides inside `source` and the
    /// butler channel inside `Playback`-adjacent `Voice::channel_index` (carried
    /// on the `Disk` construction), so dawai speaks ONE add command for both
    /// tiers.
    AddVoice {
        id: SlotId,
        /// Boxed: a [`Voice`] carries a whole `SamplerUnit`/`StreamingClipReader`,
        /// far larger than the other command variants — boxing keeps the bounded
        /// command channel's per-slot footprint small. Cold path (drained off the
        /// per-sample loop), so the indirection costs nothing audible.
        voice: Box<Voice>,
    },
    Remove(SlotId),
    ReplaceWave {
        id: SlotId,
        wave: Arc<Wave>,
    },
    UpdatePlacement {
        id: SlotId,
        start_beat: Beat,
        duration_beats: Option<BeatDuration>,
    },
    UpdateGain {
        id: SlotId,
        gain: Linear,
    },
    UpdateSpeed {
        id: SlotId,
        speed: PlaybackRate,
    },
    UpdateLoop {
        id: SlotId,
        looping: bool,
        loop_start: SamplePosition,
        loop_end: SamplePosition,
        crossfade_samples: usize,
    },
    ClearLoop(SlotId),
    UpdateReverse {
        id: SlotId,
        direction: Direction,
    },
    UpdateStretch {
        id: SlotId,
        stretch_factor: StretchFactor,
        pitch_cents: Cents,
    },
}

// Hand-rolled: `ReplaceWave` carries a non-`Debug` `Arc<Wave>` (summarize it by
// frame count); `AddVoice`'s `Box<Voice>` is `Debug`, forwarded as-is.
impl std::fmt::Debug for ClipCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddVoice { id, voice } => f
                .debug_struct("AddVoice")
                .field("id", id)
                .field("voice", voice)
                .finish(),
            Self::Remove(id) => f.debug_tuple("Remove").field(id).finish(),
            Self::ReplaceWave { id, wave } => f
                .debug_struct("ReplaceWave")
                .field("id", id)
                .field("wave_frames", &wave.len())
                .finish(),
            Self::UpdatePlacement {
                id,
                start_beat,
                duration_beats,
            } => f
                .debug_struct("UpdatePlacement")
                .field("id", id)
                .field("start_beat", start_beat)
                .field("duration_beats", duration_beats)
                .finish(),
            Self::UpdateGain { id, gain } => f
                .debug_struct("UpdateGain")
                .field("id", id)
                .field("gain", gain)
                .finish(),
            Self::UpdateSpeed { id, speed } => f
                .debug_struct("UpdateSpeed")
                .field("id", id)
                .field("speed", speed)
                .finish(),
            Self::UpdateLoop {
                id,
                looping,
                loop_start,
                loop_end,
                crossfade_samples,
            } => f
                .debug_struct("UpdateLoop")
                .field("id", id)
                .field("looping", looping)
                .field("loop_start", loop_start)
                .field("loop_end", loop_end)
                .field("crossfade_samples", crossfade_samples)
                .finish(),
            Self::ClearLoop(id) => f.debug_tuple("ClearLoop").field(id).finish(),
            Self::UpdateReverse { id, direction } => f
                .debug_struct("UpdateReverse")
                .field("id", id)
                .field("direction", direction)
                .finish(),
            Self::UpdateStretch {
                id,
                stretch_factor,
                pitch_cents,
            } => f
                .debug_struct("UpdateStretch")
                .field("id", id)
                .field("stretch_factor", stretch_factor)
                .field("pitch_cents", pitch_cents)
                .finish(),
        }
    }
}

// ---------------------------------------------------------------------------
// ClipSpec — a fully-described in-memory clip for synchronous insertion.
// ---------------------------------------------------------------------------

/// One in-memory clip, ready to drop into a reader's slot list without going
/// through the command channel. Used by the offline region render, which
/// populates a cloned (never-ticked) reader directly from ECS state.
///
/// This is the synchronous mirror of [`ClipCommand::AddVoice`]: its fields are
/// the flat Voice shape (`sampler` → the RAM [`VoiceSource`]; `direction` /
/// `stretch_factor` / `pitch_cents` → [`Playback`]). [`ClipSpec::into_voice`]
/// folds it — plus the gain / speed / loop the caller already baked onto the
/// `sampler` — into a full [`Voice`], so a single [`TrackClipReaderUnit::insert_clip`]
/// reproduces the entire live state (matching the `Add` shim's fold), not just
/// direction + stretch.
///
/// (Kept as flat fields rather than a nested `Voice` so its existing offline
/// callers build it unchanged; the L4 spectral pass migrates those call sites.)
#[derive(Debug)]
pub struct ClipSpec {
    pub id: SlotId,
    /// Already transport-bound, with gain / loop range applied.
    pub sampler: SamplerUnit,
    pub direction: Direction,
    pub stretch_factor: StretchFactor,
    pub pitch_cents: Cents,
}

impl ClipSpec {
    /// Fold this spec into a full in-RAM [`Voice`]. Captures the gain / speed /
    /// loop the caller pre-baked onto the `sampler` INTO the [`Playback`] record,
    /// the same way the `Add` shim folds a pre-configured `SamplerUnit` — so the
    /// synchronous insert applies gain at the Voice level (the `SamplerUnit` is a
    /// pure producer once its gain rides on `Playback`).
    fn into_voice(self) -> Voice {
        Voice {
            play: Playback {
                gain: self.sampler.gain(),
                speed: self.sampler.speed(),
                loop_: self.sampler.loop_setting(),
                direction: self.direction,
                stretch: self.stretch_factor,
                pitch: self.pitch_cents,
                placement: None,
            },
            source: VoiceSource::Ram(self.sampler),
            channel_index: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Handle — held by ECS systems, sends commands to the audio-thread unit.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct TrackClipReaderHandle {
    tx: Sender<ClipCommand>,
}

impl TrackClipReaderHandle {
    pub fn send(&self, cmd: ClipCommand) {
        match self.tx.try_send(cmd) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                #[cfg(feature = "bevy")]
                bevy_log::warn!("TrackClipReader command queue full, dropping command");
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// ECS components — live on the track entity.
// ---------------------------------------------------------------------------

#[cfg(feature = "bevy")]
#[derive(Component, Debug)]
pub struct TrackClipReaderRef(pub TrackClipReaderHandle);

#[cfg(feature = "bevy")]
#[derive(Component, Debug, Clone, Copy)]
pub struct TrackClipReaderNode(pub tutti_core::NodeId);

// ---------------------------------------------------------------------------
// TrackClipReaderUnit — the AudioUnit.
// ---------------------------------------------------------------------------

pub struct TrackClipReaderUnit {
    clips: Vec<ClipSlot>,
    rx: Receiver<ClipCommand>,
    sample_rate: f64,
    transport: Option<Arc<dyn Timeline>>,
    /// Typed butler write handle. `Some` on the live path (threaded in from the
    /// [`Sampler`](crate::Sampler)); `None` for tests / detached / offline
    /// readers with no live butler. Used by the drain to forward *streaming*
    /// loop ops (`Command::Loop`) — loop is butler-owned and not reachable from
    /// the reader itself. Cloning it is cheap (a `Sender` + an `Arc` map).
    butler: Option<Commands>,

    /// Output width — this node's `outputs()`, fixed at construction.
    ///
    /// Declared rather than inferred from the clips it holds: this unit is built
    /// on track creation, *before* any clip exists, and `Net` edges are wired
    /// against `outputs()`. A width that followed its contents would re-arity a
    /// live graph node the moment a clip landed.
    channels: usize,
}

// Hand-rolled: `clips` holds non-`Debug` `ClipSlot`s (each wraps a sampler +
// stretch DSP) and `transport` is an `Arc<dyn Timeline>`. Print the slot
// count + scalars rather than the slot internals.
impl std::fmt::Debug for TrackClipReaderUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackClipReaderUnit")
            .field("clips", &self.clips.len())
            .field("sample_rate", &self.sample_rate)
            .field("has_transport", &self.transport.is_some())
            .field("has_butler", &self.butler.is_some())
            .finish_non_exhaustive()
    }
}

impl TrackClipReaderUnit {
    /// Build a unit from an already-created command receiver + optional
    /// transport. Shared field-literal source for `new` / `with_transport` /
    /// `detached`.
    fn from_parts(
        rx: Receiver<ClipCommand>,
        transport: Option<Arc<dyn Timeline>>,
        butler: Option<Commands>,
    ) -> Self {
        Self::from_parts_with_channels(rx, transport, butler, 2)
    }

    fn from_parts_with_channels(
        rx: Receiver<ClipCommand>,
        transport: Option<Arc<dyn Timeline>>,
        butler: Option<Commands>,
        channels: usize,
    ) -> Self {
        Self {
            clips: Vec::new(),
            rx,
            sample_rate: 44100.0,
            transport,
            butler,
            channels: channels.max(1),
        }
    }

    /// Output width — this node's `outputs()`.
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn new() -> (Self, TrackClipReaderHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let handle = TrackClipReaderHandle { tx };
        (Self::from_parts(rx, None, None), handle)
    }

    pub fn with_transport(
        transport: Arc<dyn Timeline>,
        butler: Option<Commands>,
    ) -> (Self, TrackClipReaderHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let handle = TrackClipReaderHandle { tx };
        (Self::from_parts(rx, Some(transport), butler), handle)
    }

    /// As [`with_transport`](Self::with_transport), at an explicit output width.
    ///
    /// Each slot's stretch unit is built at this width too, so a wide clip is
    /// not truncated on the stretch path.
    pub fn with_channels(
        transport: Option<Arc<dyn Timeline>>,
        butler: Option<Commands>,
        channels: usize,
    ) -> (Self, TrackClipReaderHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let handle = TrackClipReaderHandle { tx };
        (
            Self::from_parts_with_channels(rx, transport, butler, channels),
            handle,
        )
    }

    /// Number of clip slots currently materialised (drained from the command
    /// queue). Diagnostic / test helper.
    pub fn clip_count(&self) -> usize {
        self.clips.len()
    }

    /// The control intent recorded for a slot. Test helper: lets a test assert
    /// that a dropped command did NOT leave a `Playback` claiming it applied.
    #[cfg(test)]
    fn playback_of(&self, id: SlotId) -> Option<&Playback> {
        self.clips
            .iter()
            .find(|s| s.id == id)
            .map(|s| &s.voice.play)
    }

    /// Build a render-only reader: empty, bound to `transport`, with no live
    /// command channel — its `rx` is a `bounded(0)` receiver that has no sender
    /// and can never deliver anything.
    ///
    /// The offline region render clones the *staged frontend* net (fundsp
    /// `Net::clone`), which clones each live reader and — by necessity, since
    /// `commit()` relies on full-fidelity cloning — shares the live crossbeam
    /// `Receiver`. A clone handed to the offline worker must never drain that
    /// channel (crossbeam delivers each command to exactly one receiver, so an
    /// offline drain steals commands from the audio thread) and must never
    /// carry live clip state. Rather than clone-then-sever, the render Prepare
    /// step *replaces* each reader node with one of these: born empty, born
    /// channel-less, so it shares zero mutable state with the live graph at any
    /// instant. Clips are then rebuilt from ECS in the Populate step via
    /// [`Self::insert_clip`].
    pub fn detached(transport: Arc<dyn Timeline>) -> Self {
        let (_tx, rx) = bounded(0);
        // No butler: the offline render never forwards streaming loop ops (it
        // rebuilds in-RAM clips from ECS), so a `None` handle is correct here.
        Self::from_parts(rx, Some(transport), None)
    }

    /// Re-point the reader at a new transport. The offline region render calls
    /// this after [`isolate`](AudioUnit::isolate) has emptied the reader, so it
    /// only needs to seat the render's transport; any clips inserted afterward
    /// (via [`insert_clip`](Self::insert_clip)) are built against it. Mirrors
    /// [`VoiceNode::replace_transport`] so both transport-aware nodes rebind the
    /// same way in the render's isolation pass.
    pub fn replace_transport(&mut self, transport: Arc<dyn Timeline>) {
        for slot in &mut self.clips {
            slot.voice.replace_transport(transport.clone());
        }
        self.transport = Some(transport);
    }

    /// Insert a clip slot directly, bypassing the command channel.
    ///
    /// The live path adds clips by sending `ClipCommand::AddVoice` and letting the
    /// audio thread drain it in `tick`/`process`. A cloned net built for the
    /// offline render is never ticked on a thread that drains, so it needs its
    /// clips materialised synchronously — that's this. Routes through the exact
    /// same [`Self::insert_voice`] path the live `AddVoice` drain uses, folding the
    /// spec (plus the sampler's pre-baked gain / speed / loop) into a full
    /// [`Voice`] via [`ClipSpec::into_voice`] — so the offline insert applies the
    /// entire live state, gain included, at the Voice level.
    pub fn insert_clip(&mut self, spec: ClipSpec) {
        let id = spec.id;
        self.insert_voice(id, spec.into_voice());
    }

    /// Drop every clip slot.
    ///
    /// The offline render clones the staged net and then rebuilds each clip
    /// fresh from ECS + the wave cache; clearing the inherited slots first
    /// keeps the cloned reader from carrying any state tied to the live graph.
    pub fn clear_clips(&mut self) {
        self.clips.clear();
    }

    fn slot_mut(&mut self, id: SlotId) -> Option<&mut ClipSlot> {
        self.clips.iter_mut().find(|s| s.id == id)
    }

    /// Apply a loop setting to a slot, routing by tier.
    ///
    /// - **In-RAM**: primes / clears the loop range on the `SamplerUnit` in-unit
    ///   (`SamplerUnit::set_loop_setting`).
    /// - **Streaming**: loop is butler-owned — `SetStreamLoop` reads a loop-start
    ///   fadein head off disk and mutates `plan.link.loop_config`, neither
    ///   reachable from the reader — so the reader FORWARDS to the butler via the
    ///   typed [`Commands`] handle (`Command::Loop`, which maps `On`→
    ///   `SetStreamLoop` / `Off`→`ClearStreamLoop`). This is the same command
    ///   dawai-model used to send itself; it now originates here so dawai speaks
    ///   one unified `ClipCommand` for both tiers.
    ///
    /// RT-safe: this runs on the COLD command drain (top of `tick`/`process`,
    /// before the per-sample loop), so the channel send is fine — it never
    /// touches the per-sample hot path.
    fn apply_loop(&mut self, id: SlotId, setting: LoopSetting) {
        let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) else {
            return;
        };
        match &mut slot.voice.source {
            VoiceSource::Ram(sampler) => {
                sampler.set_loop_setting(setting.clone());
                slot.voice.play.loop_ = setting;
            }
            VoiceSource::Disk(_) => {
                // Streaming loop is butler-owned, so this needs both a butler
                // handle and a registered channel. A reader built without one
                // (`new()`, `detached()`, `isolate()` — the offline/render
                // paths) has neither.
                let Some((butler, channel_index)) =
                    self.butler.as_ref().zip(slot.voice.channel_index)
                else {
                    // Do NOT record the intent: the butler was never told, and
                    // a `play.loop_` that says "looping" while the stream is
                    // not would make `Playback` lie about the applied state —
                    // which `insert_voice` then replays as if it were real.
                    #[cfg(feature = "bevy")]
                    bevy_log::warn!(
                        "loop command dropped for slot {id:?}: streaming voice has no butler channel"
                    );
                    return;
                };
                butler.send(Command::Loop {
                    channel_index,
                    setting: setting.clone(),
                });
                slot.voice.play.loop_ = setting;
            }
        }
    }

    /// Insert a fully-built [`Voice`] as a new slot and REALISE its full
    /// `Playback` intent per-tier — the single path the [`AddVoice`] command
    /// funnels through. Public so a clip-aware caller (the offline region
    /// render's `Populate` step) can hand the reader a `Voice` it built from
    /// ECS DATA — gain / loop / direction / stretch / pitch carried on
    /// `voice.play` — instead of pre-poking a `SamplerUnit` before send.
    ///
    /// The `Playback` is control-INTENT; each tier applies it its own way. Rather
    /// than duplicate the tier fork, we replay the exact cold-path appliers the
    /// `Update*` commands use: `VoiceSource::apply_*` (per-tier match — in-RAM
    /// stores on the `SamplerUnit`, streaming forwards to the shared `RtState`)
    /// for gain / speed / direction, and `apply_loop` for loop (in-RAM
    /// primes the range in-unit, streaming forwards `Command::Loop` to the
    /// butler). Stretch/pitch are primed by `ClipSlot::new` from `play`. Runs on
    /// the COLD command drain, so the loop's butler send is RT-safe.
    ///
    /// [`AddVoice`]: ClipCommand::AddVoice
    pub fn insert_voice(&mut self, id: SlotId, voice: Voice) {
        self.clips.retain(|s| s.id != id);
        // Split the loop out: `apply_loop` needs the slot present to look it up,
        // and `Playback` moves into the `Voice`. Take the rest by copy first.
        let loop_ = voice.play.loop_.clone();
        let gain = voice.play.gain;
        let speed = voice.play.speed;
        let direction = voice.play.direction;
        // `ClipSlot::with_channels` primes the resident stretch unit from
        // `voice.play` (stretch/pitch) — the one heavy step, done here off the
        // hot path. It is built at THIS READER's width: a slot narrower than the
        // reader would truncate on the stretch path only, which no stereo test
        // can observe.
        self.clips.push(ClipSlot::with_channels(
            id,
            voice,
            self.sample_rate,
            self.channels,
        ));

        // Realise the remaining intent through the same appliers the update
        // commands use. The slot is now present, so `slot_mut` / `apply_loop`
        // find it by id.
        if let Some(slot) = self.slot_mut(id) {
            slot.voice.source.apply_gain(gain);
            slot.voice.play.gain = gain;
            slot.voice.source.apply_speed(speed);
            slot.voice.play.speed = speed;
            slot.voice.play.direction = direction;
            slot.voice.source.apply_direction(direction);
        }
        self.apply_loop(id, loop_);
    }

    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                ClipCommand::AddVoice { id, voice } => {
                    self.insert_voice(id, *voice);
                }
                ClipCommand::Remove(id) => {
                    self.clips.retain(|s| s.id != id);
                }
                ClipCommand::ReplaceWave { id, wave } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // Match the tier explicitly rather than calling through
                        // a shared setter whose streaming half was a silent
                        // no-op: a `ReplaceWave` aimed at a disk voice did
                        // nothing and said nothing. Swapping a streaming
                        // source means re-registering the butler stream on a
                        // different file, which is a control-thread op issued
                        // from dawai-model as a fresh `AddVoice`.
                        match &mut slot.voice.source {
                            VoiceSource::Ram(sampler) => sampler.set_wave(wave),
                            VoiceSource::Disk(_) => {
                                #[cfg(feature = "bevy")]
                                bevy_log::warn!(
                                    "ReplaceWave ignored for slot {id:?}: a streaming voice \
                                     changes source by re-issuing AddVoice, not in-unit"
                                );
                            }
                        }
                    }
                }
                ClipCommand::UpdatePlacement {
                    id,
                    start_beat,
                    duration_beats,
                } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // Both backends carry a placement gate; each backend's
                        // `set_placement` re-arms its own (streaming re-seeks on
                        // the next inside-frame).
                        slot.voice
                            .source
                            .apply_placement(start_beat, duration_beats);
                        if let Some(placement) = &mut slot.voice.play.placement {
                            placement.start_beat = start_beat;
                            placement.duration_beats = duration_beats;
                        }
                    }
                }
                ClipCommand::UpdateGain { id, gain } => {
                    if let Some(slot) = self.slot_mut(id) {
                        slot.voice.source.apply_gain(gain);
                        slot.voice.play.gain = gain;
                    }
                }
                ClipCommand::UpdateSpeed { id, speed } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // Unified across tiers: both backends carry speed in-unit.
                        // In-RAM stores it on the `SamplerUnit`; streaming forwards
                        // to the shared `RtState` (the exact speed effect of the
                        // butler's `SetVarispeed`, reachable from the reader). The
                        // `apply_speed` covers both — dawai no longer
                        // forks streaming speed onto a separate butler command.
                        slot.voice.source.apply_speed(speed);
                        slot.voice.play.speed = speed;
                    }
                }
                ClipCommand::UpdateLoop {
                    id,
                    looping,
                    loop_start,
                    loop_end,
                    crossfade_samples,
                } => {
                    let setting = if looping {
                        LoopSetting::On {
                            start: loop_start,
                            end: loop_end,
                            crossfade_samples,
                        }
                    } else {
                        LoopSetting::Off
                    };
                    self.apply_loop(id, setting);
                }
                ClipCommand::ClearLoop(id) => {
                    self.apply_loop(id, LoopSetting::Off);
                }
                ClipCommand::UpdateReverse { id, direction } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // In-RAM: `voice.play.direction` drives the reversed index
                        // in the hot read (the source-side `set_direction` is a
                        // no-op). Streaming: the source-side `set_direction`
                        // forwards to the shared `RtState` (the direction leg of the
                        // butler's `SetVarispeed`) — `voice.play.direction` is
                        // unused by the ring pull. One command reaches both, so
                        // dawai sends reverse ONCE, no longer folding it into a
                        // separate butler speed command.
                        slot.voice.play.direction = direction;
                        slot.voice.source.apply_direction(direction);
                    }
                }
                ClipCommand::UpdateStretch {
                    id,
                    stretch_factor,
                    pitch_cents,
                } => {
                    if let Some(slot) = self.slot_mut(id) {
                        slot.set_stretch(stretch_factor, pitch_cents);
                    }
                }
            }
        }
    }
}

impl Clone for TrackClipReaderUnit {
    fn clone(&self) -> Self {
        // `AudioUnit: DynClone`, so the graph (fundsp `Net`) clones this unit on
        // commit (frontend↔backend mem-swap) and may clone it on realloc. The
        // clone therefore MUST keep receiving the commands the ECS handle's
        // `Sender` still feeds — so we share the *same* `Receiver` rather than
        // minting a fresh, dead channel. `crossbeam` delivers each message to
        // exactly one receiver, and the live graph only ever ticks one instance
        // at a time, so there is no double-drain.
        //
        // The offline region render does NOT rely on this: it replaces each
        // reader node with a fresh [`Self::detached`] (channel-less) reader in
        // its Prepare step, so a render clone never shares this `Receiver` while
        // being ticked on a worker thread.
        Self {
            clips: self
                .clips
                .iter()
                .map(|s| ClipSlot {
                    id: s.id,
                    voice: s.voice.clone(),
                    stretch: s.stretch.clone(),
                    sample_rate: s.sample_rate,
                }) // Voice (VoiceSource) + resident stretch::Unit clone by value; atomics preserved
                .collect(),
            rx: self.rx.clone(),
            sample_rate: self.sample_rate,
            transport: self.transport.clone(),
            butler: self.butler.clone(),
            channels: self.channels,
        }
    }
}

impl AudioUnit for TrackClipReaderUnit {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        self.channels
    }

    fn reset(&mut self) {
        for slot in &mut self.clips {
            slot.voice.source.as_audio_unit_mut().reset();
            slot.stretch.reset();
        }
    }

    /// Sever the live command channel and clear live clip state so an offline
    /// clone can be ticked on a worker thread without stealing commands from —
    /// or sharing clips with — the live reader. Leaves the unit *born empty and
    /// channel-less* (the [`detached`](Self::detached) state), minus the
    /// transport: re-pointing at the render's offline transport is the caller's
    /// separate data-carrying step (via [`replace_transport`](Self::replace_transport)),
    /// per the `isolate` contract.
    fn isolate(&mut self) {
        // A `bounded(0)` receiver whose sender is dropped can never deliver, so
        // the worker's drain sees nothing and steals nothing from the live rx.
        let (_tx, rx) = bounded(0);
        self.rx = rx;
        self.clips.clear();
        // The offline render rebuilds clips from ECS and never forwards
        // streaming loop ops, so it needs no butler handle.
        self.butler = None;
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate.get();
        for slot in &mut self.clips {
            slot.voice
                .source
                .as_audio_unit_mut()
                .set_sample_rate(sample_rate);
            slot.sample_rate = sample_rate.get();
            slot.stretch.set_sample_rate(sample_rate);
        }
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        self.drain_commands();

        let n = self.channels.min(output.len()).min(MAX_SAMPLER_CHANNELS);
        if n == 0 {
            return;
        }
        output[..n].fill(0.0);

        // Each slot reads its ONE voice via the shared
        // `ClipSlot::tick_frame_into` (the same per-variant `VoiceSource` match
        // a standalone `VoiceNode` uses — factored, not duplicated, and no
        // per-sample dyn), summed channel-wise into the caller's frame.
        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
        for slot in &mut self.clips {
            slot.tick_frame_into(&mut frame[..n]);
            for (c, &s) in frame.iter().enumerate().take(n) {
                output[c] += s;
            }
        }
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        self.drain_commands();

        let n = self
            .channels
            .min(output.channels())
            .min(MAX_SAMPLER_CHANNELS);
        for c in 0..n {
            for i in 0..size {
                output.set_f32(c, i, 0.0);
            }
        }

        // Each slot accumulates its ONE voice via the shared
        // `ClipSlot::process_into` (the same per-variant `VoiceSource` match a
        // standalone `VoiceNode` uses).
        for slot in &mut self.clips {
            slot.process_into(size, n, output);
        }
    }

    audio_unit_boilerplate!(id = TRACK_CLIP_READER_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        SignalFrame::new(self.channels)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>() + self.clips.len() * std::mem::size_of::<ClipSlot>()
    }
}

// ---------------------------------------------------------------------------
// VoiceNode — a standalone single-`Voice` graph node (0 inputs, 2 outputs).
//
// The mixer (`TrackClipReaderUnit`) holds a *list* of voices and sums them; a
// `VoiceNode` holds exactly ONE and plays it — a degenerate single-voice
// mixdown. Its `tick`/`process` are the SAME per-voice read the mixer does for
// one slot, shared through [`ClipSlot::tick_frame`] / [`ClipSlot::process_into`]
// (no duplication, no per-sample dyn).
//
// This is the standalone-graph-node case: `dawai-spectral`'s resynth adds a bare
// voice node to its net, and `tutti-export`'s region render downcasts these
// nodes to rebind their transport offline. Both rely on a `Voice` being an
// `AudioUnit` in its own right — not only reachable through the mixer — so this
// node keeps that path alive (guarded by a test).
// ---------------------------------------------------------------------------

pub struct VoiceNode {
    slot: ClipSlot,
    /// Output width — see [`TrackClipReaderUnit`]'s field of the same name.
    channels: usize,
}

impl VoiceNode {
    /// Wrap a single [`Voice`] as a standalone **stereo** graph node. Builds the
    /// resident stretch processor once (like a mixer slot), off any hot path.
    pub fn new(voice: Voice) -> Self {
        Self::with_channels(voice, 2)
    }

    /// Wrap a single [`Voice`] as a `channels`-wide graph node.
    pub fn with_channels(voice: Voice, channels: usize) -> Self {
        let channels = channels.max(1);
        Self {
            slot: ClipSlot::with_channels(SlotId(0), voice, 44100.0, channels),
            channels,
        }
    }

    /// Output width — this node's `outputs()`.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// The wrapped voice (immutable view).
    pub fn voice(&self) -> &Voice {
        &self.slot.voice
    }

    /// The wrapped voice (mutable view) — used by the offline render to rebind
    /// the transport via [`Voice::replace_transport`].
    pub fn voice_mut(&mut self) -> &mut Voice {
        &mut self.slot.voice
    }

    /// Rebind the transport clock behind the wrapped voice's placement,
    /// preserving start / duration. Convenience delegate to
    /// [`Voice::replace_transport`] so the offline region render can rebind a
    /// standalone voice node without reaching through `voice_mut`.
    pub fn replace_transport(&mut self, transport: Arc<dyn Timeline>) {
        self.slot.voice.replace_transport(transport);
    }
}

// Hand-rolled: wraps a non-`Debug` `ClipSlot`. Print the wrapped `Voice`
// (which is `Debug`) and leave the resident stretch DSP out.
impl std::fmt::Debug for VoiceNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceNode")
            .field("voice", &self.slot.voice)
            .finish_non_exhaustive()
    }
}

impl From<Voice> for VoiceNode {
    fn from(voice: Voice) -> Self {
        Self::new(voice)
    }
}

impl Clone for VoiceNode {
    fn clone(&self) -> Self {
        Self {
            slot: ClipSlot {
                id: self.slot.id,
                voice: self.slot.voice.clone(),
                stretch: self.slot.stretch.clone(),
                sample_rate: self.slot.sample_rate,
            },
            channels: self.channels,
        }
    }
}

impl AudioUnit for VoiceNode {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        self.channels
    }

    fn reset(&mut self) {
        self.slot.voice.source.as_audio_unit_mut().reset();
        self.slot.stretch.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.slot
            .voice
            .source
            .as_audio_unit_mut()
            .set_sample_rate(sample_rate);
        self.slot.sample_rate = sample_rate.get();
        self.slot.stretch.set_sample_rate(sample_rate);
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        // Same single-voice read the mixer runs per slot, straight into the
        // caller's frame.
        let n = self.channels.min(output.len());
        if n == 0 {
            return;
        }
        self.slot.tick_frame_into(&mut output[..n]);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        let n = self
            .channels
            .min(output.channels())
            .min(MAX_SAMPLER_CHANNELS);
        for c in 0..n {
            for i in 0..size {
                output.set_f32(c, i, 0.0);
            }
        }
        self.slot.process_into(size, n, output);
    }

    audio_unit_boilerplate!(id = crate::node_id::VOICE_NODE_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        SignalFrame::new(self.channels)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::sampler_unit::SamplerUnitConfig;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct MockTransport {
        playing: AtomicBool,
        beat: AtomicU64,
        tempo: AtomicU64,
    }

    impl MockTransport {
        fn new(tempo: f64, beat: f64, playing: bool) -> Arc<Self> {
            Arc::new(Self {
                playing: AtomicBool::new(playing),
                beat: AtomicU64::new(beat.to_bits()),
                tempo: AtomicU64::new(tempo.to_bits()),
            })
        }
    }

    impl Timeline for MockTransport {
        fn is_rolling(&self) -> bool {
            self.playing.load(Ordering::Relaxed)
        }
        fn beat(&self) -> tutti_core::Beat {
            tutti_core::Beat(f64::from_bits(self.beat.load(Ordering::Relaxed)))
        }
        fn tempo(&self) -> tutti_core::Bpm {
            tutti_core::Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
        }
    }

    fn make_wave(samples: usize) -> Arc<Wave> {
        let data: Vec<f32> = (0..samples)
            .map(|i| (i as f32 + 1.0) / samples as f32)
            .collect();
        Arc::new(Wave::from_samples(44100.0, &data))
    }

    /// Send an in-RAM clip through the one `AddVoice` path — the test-setup
    /// mirror of the timeline's `promote_pending_clip_waves` emit.
    fn add_ram_clip(handle: &TrackClipReaderHandle, id: SlotId, sampler: SamplerUnit) {
        handle.send(ClipCommand::AddVoice {
            id,
            voice: Box::new(Voice {
                source: VoiceSource::Ram(sampler),
                play: Playback::default(),
                channel_index: None,
            }),
        });
    }

    #[test]
    fn add_and_remove_clips() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        let sampler =
            SamplerUnit::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(out[0] != 0.0 || out[1] != 0.0, "clip should produce audio");

        handle.send(ClipCommand::Remove(SlotId(1)));
        unit.tick(&[], &mut out);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn silence_when_transport_stopped() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, false);
        let wave = make_wave(100);

        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn clips_sum_together() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        for i in 0..3 {
            let sampler =
                SamplerUnit::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
            add_ram_clip(&handle, SlotId(i), sampler);
        }

        let mut out_3 = [0.0f32; 2];
        unit.tick(&[], &mut out_3);

        let (mut unit2, handle2) = TrackClipReaderUnit::new();
        let sampler =
            SamplerUnit::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
        add_ram_clip(&handle2, SlotId(0), sampler);
        let mut out_1 = [0.0f32; 2];
        unit2.tick(&[], &mut out_1);

        let tolerance = 0.001;
        assert!((out_3[0] - out_1[0] * 3.0).abs() < tolerance);
        assert!((out_3[1] - out_1[1] * 3.0).abs() < tolerance);
    }

    #[test]
    fn clone_snapshots_clips() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);

        let mut cloned = unit.clone();
        let mut out_clone = [0.0f32; 2];
        cloned.tick(&[], &mut out_clone);

        assert!(out_clone[0] != 0.0, "cloned unit should have the clip");
    }

    #[test]
    fn insert_clip_is_audible_without_channel() {
        let (mut unit, _handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        unit.insert_clip(ClipSpec {
            id: SlotId(1),
            sampler,
            direction: Direction::Forward,
            stretch_factor: StretchFactor::new(1.0),
            pitch_cents: Cents::new(0.0),
        });

        // No tick/drain needed — the clip is already in the slot list.
        assert_eq!(unit.clip_count(), 1);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(
            out[0] != 0.0 || out[1] != 0.0,
            "inserted clip should produce audio"
        );
    }

    #[test]
    fn detached_reader_has_no_channel_and_is_empty() {
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        // A render-only reader is born empty and channel-less: there is no
        // sender for its `rx`, so no command can ever reach it.
        let mut unit = TrackClipReaderUnit::detached(transport.clone());
        assert_eq!(unit.clip_count(), 0, "detached reader starts empty");

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out); // drains its (permanently empty) queue
        assert_eq!(unit.clip_count(), 0, "detached reader receives no commands");
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);

        // But clips inserted directly (the Populate path) are audible.
        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        unit.insert_clip(ClipSpec {
            id: SlotId(1),
            sampler,
            direction: Direction::Forward,
            stretch_factor: StretchFactor::new(1.0),
            pitch_cents: Cents::new(0.0),
        });
        unit.tick(&[], &mut out);
        assert!(out[0] != 0.0 || out[1] != 0.0, "inserted clip is audible");
    }

    #[test]
    fn update_gain() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out_before = [0.0f32; 2];
        unit.tick(&[], &mut out_before);

        handle.send(ClipCommand::UpdateGain {
            id: SlotId(1),
            gain: Linear::new(0.5),
        });

        let mut out_after = [0.0f32; 2];
        unit.tick(&[], &mut out_after);

        assert!((out_after[0] - out_before[0] * 0.5).abs() < 0.01);
    }

    #[test]
    fn update_stretch_enables_processor() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(4096);

        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(!unit.clips[0].needs_stretch(), "no stretch by default");

        handle.send(ClipCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(2.0),
            pitch_cents: Cents::new(0.0),
        });
        unit.tick(&[], &mut out);
        assert!(unit.clips[0].needs_stretch(), "stretch should be active");

        handle.send(ClipCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(1.0),
            pitch_cents: Cents::new(0.0),
        });
        unit.tick(&[], &mut out);
        assert!(
            !unit.clips[0].needs_stretch(),
            "identity stretch disables processor"
        );
    }

    /// `VoiceNode` MUST be an `AudioUnit` in its own right: `dawai-spectral`'s
    /// resynth adds a standalone voice node to its net, and `tutti-export`'s
    /// region render downcasts these nodes to rebind them offline. If a `Voice`
    /// stopped being an `AudioUnit`, both paths would break — resynth couldn't
    /// add it, and the offline downcast would silently stop matching (wrong
    /// transport offline, no compile error). This guards that contract: a
    /// standalone `VoiceNode` produces the same audio one mixer slot does.
    #[test]
    fn voice_node_is_a_standalone_audio_unit() {
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        // Same voice the mixer would hold for one clip.
        let sampler =
            SamplerUnit::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
        let voice = Voice {
            source: VoiceSource::Ram(sampler),
            play: Playback::default(),
            channel_index: None,
        };

        // As a standalone AudioUnit: 0 in, 2 out, produces audio.
        let mut node = VoiceNode::new(voice);
        assert_eq!(node.inputs(), 0);
        assert_eq!(node.outputs(), 2);

        let mut out = [0.0f32; 2];
        node.tick(&[], &mut out);
        assert!(
            out[0] != 0.0 || out[1] != 0.0,
            "standalone VoiceNode should produce audio"
        );

        // It reads the SAME single-voice frame the mixer does for one slot.
        let sampler2 = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        let (mut mixer, _handle) = TrackClipReaderUnit::new();
        mixer.insert_clip(ClipSpec {
            id: SlotId(1),
            sampler: sampler2,
            direction: Direction::Forward,
            stretch_factor: StretchFactor::new(1.0),
            pitch_cents: Cents::new(0.0),
        });
        let mut node2 = VoiceNode::new(Voice {
            source: VoiceSource::Ram(SamplerUnit::with_transport(
                make_wave(100),
                MockTransport::new(120.0, 0.0, true),
                Beat::new(0.0),
                None,
            )),
            play: Playback::default(),
            channel_index: None,
        });
        let mut mixer_out = [0.0f32; 2];
        let mut node_out = [0.0f32; 2];
        mixer.tick(&[], &mut mixer_out);
        node2.tick(&[], &mut node_out);
        assert!(
            (mixer_out[0] - node_out[0]).abs() < 1e-6,
            "VoiceNode frame must match the mixer's single-slot read"
        );
    }

    /// `Voice::replace_transport` rebinds the placement clock (the offline
    /// render's rebind path), preserving start/duration.
    #[test]
    fn voice_replace_transport_rebinds_clock() {
        let wave = make_wave(100);
        let t1 = MockTransport::new(120.0, 0.0, true);
        let sampler = SamplerUnit::with_transport(wave, t1, Beat::new(0.0), None);
        let mut voice = Voice {
            source: VoiceSource::Ram(sampler),
            play: Playback {
                placement: Some(TransportPlacement {
                    transport: MockTransport::new(120.0, 0.0, true),
                    start_beat: Beat::new(2.0),
                    duration_beats: None,
                }),
                ..Playback::default()
            },
            channel_index: None,
        };

        let t2 = MockTransport::new(140.0, 1.0, false);
        voice.replace_transport(t2);
        let placement = voice.play.placement.as_ref().expect("placement present");
        assert_eq!(placement.start_beat, Beat::new(2.0));
        assert!(
            !placement.transport.is_rolling(),
            "clock swapped to the stopped one"
        );
    }

    // --- 0e: dropped commands must not be recorded as applied ---

    /// A streaming voice with no butler channel cannot have its loop applied —
    /// the butler owns streaming loop state. The drain used to send nothing and
    /// still write `play.loop_`, so the intent record claimed a loop that was
    /// never set; `insert_voice` would then replay that lie. Reachable on every
    /// offline path: `new()`, `detached()`, and `isolate()` all have no butler.
    #[test]
    fn loop_on_a_butlerless_streaming_voice_is_not_recorded() {
        use crate::butler::{share_reader, RegionBuffer, RegionId, RtState};
        use crate::clip::streaming_sampler::StreamingSamplerUnit;
        use crate::clip::streaming_sampler::{StreamingClipConfig, StreamingClipReader};

        let transport = MockTransport::new(120.0, 0.0, true);
        let (mut unit, handle) = TrackClipReaderUnit::new();

        // Build a Disk voice with no butler channel.
        let (writer, reader) =
            RegionBuffer::with_capacity(RegionId(1), std::path::PathBuf::new(), 128);
        drop(writer);
        let state = std::sync::Arc::new(RtState::new());
        let inner = StreamingSamplerUnit::new(share_reader(reader), state.clone());
        let clip_reader = StreamingClipReader::new(
            inner,
            state,
            StreamingClipConfig {
                placement: TransportPlacement {
                    transport: transport.clone(),
                    start_beat: Beat::new(0.0),
                    duration_beats: None,
                },
                file_sample_rate: 44100.0,
            },
        );

        let id = SlotId(7);
        handle.send(ClipCommand::AddVoice {
            id,
            voice: Box::new(Voice {
                source: VoiceSource::Disk(clip_reader),
                play: Playback::default(),
                channel_index: None,
            }),
        });

        handle.send(ClipCommand::UpdateLoop {
            id,
            looping: true,
            loop_start: SamplePosition::new(0.0),
            loop_end: SamplePosition::new(64.0),
            crossfade_samples: 0,
        });

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);

        let play = unit.playback_of(id).expect("slot exists");
        assert_eq!(
            play.loop_,
            LoopSetting::Off,
            "a loop the butler was never told about must not be recorded as applied"
        );
    }

    /// Channel `c` carries the constant `c + 1`.
    fn indexed_wave(channels: usize, len: usize) -> Arc<Wave> {
        let mut w = Wave::zero(channels, 44_100.0, len as f64 / 44_100.0);
        for i in 0..w.len() {
            for c in 0..channels {
                w.set(c, i, (c + 1) as f32);
            }
        }
        Arc::new(w)
    }

    #[test]
    fn reader_and_voice_node_default_to_stereo() {
        let (unit, _h) = TrackClipReaderUnit::new();
        assert_eq!(unit.channels(), 2);
        assert_eq!(unit.outputs(), 2);
    }

    /// `route`'s width must track `outputs()` on both nodes, or fundsp mis-plans
    /// their latency — silent except as PDC drift.
    #[test]
    fn route_width_tracks_outputs_on_both_nodes() {
        for w in [1usize, 2, 6, 8] {
            let (mut unit, _h) = TrackClipReaderUnit::with_channels(None, None, w);
            let out = unit.route(&SignalFrame::new(0), 44_100.0);
            assert_eq!(
                out.len(),
                unit.outputs(),
                "reader route/outputs at width {w}"
            );

            let transport = MockTransport::new(120.0, 0.0, true);
            let sampler = SamplerUnit::with_config(
                indexed_wave(6, 64),
                SamplerUnitConfig {
                    channels: w,
                    placement: Some(TransportPlacement {
                        transport,
                        start_beat: Beat::new(0.0),
                        duration_beats: None,
                    }),
                    ..Default::default()
                },
            );
            let voice = Voice {
                source: VoiceSource::Ram(sampler),
                play: Playback::default(),
                channel_index: None,
            };
            let mut vn = VoiceNode::with_channels(voice, w);
            let out = vn.route(&SignalFrame::new(0), 44_100.0);
            assert_eq!(
                out.len(),
                vn.outputs(),
                "voice node route/outputs at width {w}"
            );
        }
    }

    /// A 6-channel clip in a 6-wide reader must reach all six outputs, through
    /// both entry points (`tick` sums into the caller's slice; `process`
    /// accumulates into a planar buffer — different code).
    #[test]
    fn six_channel_clip_reaches_all_six_reader_outputs() {
        let transport = MockTransport::new(120.0, 0.0, true);
        let (mut unit, _h) = TrackClipReaderUnit::with_channels(Some(transport.clone()), None, 6);
        let sampler = SamplerUnit::with_config(
            indexed_wave(6, 512),
            SamplerUnitConfig {
                channels: 6,
                placement: Some(TransportPlacement {
                    transport,
                    start_beat: Beat::new(0.0),
                    duration_beats: None,
                }),
                ..Default::default()
            },
        );
        unit.insert_voice(
            SlotId(1),
            Voice {
                source: VoiceSource::Ram(sampler),
                play: Playback::default(),
                channel_index: None,
            },
        );

        let mut out = [0.0f32; 6];
        unit.tick(&[], &mut out);
        for (c, &got) in out.iter().enumerate() {
            assert!(
                (got - (c + 1) as f32).abs() < 1e-3,
                "tick: channel {c} should carry {}, got {got} ({out:?})",
                c + 1
            );
        }
        assert!(
            out[2..].iter().all(|&s| s.abs() > 0.5),
            "channels 2..6 were dropped: {out:?}"
        );
    }

    /// The stretch branch is separate code from the direct read, and it is the
    /// one R6 warns about: a slot whose stretcher is narrower than the reader
    /// truncates silently, and ONLY when stretch is enabled. Nothing else in the
    /// suite exercises that combination at width 6.
    #[test]
    fn six_channel_clip_with_stretch_reaches_all_six_outputs() {
        let transport = MockTransport::new(120.0, 0.0, true);
        let (mut unit, _h) = TrackClipReaderUnit::with_channels(Some(transport.clone()), None, 6);
        let sampler = SamplerUnit::with_config(
            indexed_wave(6, 4096),
            SamplerUnitConfig {
                channels: 6,
                placement: Some(TransportPlacement {
                    transport: transport.clone(),
                    start_beat: Beat::new(0.0),
                    duration_beats: None,
                }),
                ..Default::default()
            },
        );
        unit.insert_voice(
            SlotId(1),
            Voice {
                source: VoiceSource::Ram(sampler),
                play: Playback {
                    // Off unity, so `needs_stretch()` takes the vocoder path.
                    stretch: StretchFactor::new(2.0),
                    ..Default::default()
                },
                channel_index: None,
            },
        );

        // The phase vocoder has FFT latency, so early frames are legitimately
        // silent; drive until every channel has produced something.
        let mut seen = [false; 6];
        let mut out = [0.0f32; 6];
        for n in 0..16_384 {
            unit.tick(&[], &mut out);
            for (c, &s) in out.iter().enumerate() {
                if s.abs() > 1e-6 {
                    seen[c] = true;
                }
            }
            if seen.iter().all(|&b| b) {
                break;
            }
            let _ = n;
        }
        assert!(
            seen.iter().all(|&b| b),
            "channels {:?} never produced output through the stretch path",
            seen.iter()
                .enumerate()
                .filter(|(_, &b)| !b)
                .map(|(c, _)| c)
                .collect::<Vec<_>>()
        );
    }
}
