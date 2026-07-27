//! Per-track voice pool: a single graph node that internally manages
//! all audio voice playback for one track.
//!
//! Replaces the old "one `MemorySource` graph node per voice + dynamic
//! `StereoSumUnit`" model. ECS systems send [`VoiceCommand`]s through
//! a [`VoicePoolHandle`]; the unit drains them each audio buffer.
//!
//! Each voice is played by a [`Voice`]: a [`VoiceSource`] (EITHER an in-memory
//! [`MemorySource`] with the whole source resident in memory as an `Arc<Wave>`, decoded
//! once by the wave cache, OR a [`DiskVoice`] that pulls incrementally
//! from the butler ring) plus its [`Playback`] control-intent record. The source
//! choice is a monomorphized [`VoiceSource`] enum, not a boxed trait object, so
//! the per-buffer match stays inlinable and the hot path allocation-free. The
//! optional time-stretch processor wraps whichever source when a voice is
//! stretched/pitched (both variants `impl AudioUnit`). A single [`Voice`] can
//! also stand on its own as a [`VoiceNode`] graph node (resynth / preview),
//! sharing the exact per-voice read the mixer does per slot.

use std::sync::Arc;

use crate::ports::{Command, Commands};
use crate::stretch;
use crate::MAX_SAMPLER_CHANNELS;

use super::disk_voice::DiskVoice;
use super::memory_source::{LoopSetting, MemorySource, TransportPlacement};
#[cfg(feature = "bevy")]
use bevy_ecs::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use tutti_core::transport::BeatCursor;
use tutti_core::{
    Amplitude, AudioUnit, Beat, BeatDuration, BufferMut, BufferRef, Cents, PlaybackRate,
    SamplePosition, SignalFrame, StretchFactor, Timeline, Wave,
};

/// Voice slots a reader holds before its slot vector has to grow.
///
/// The `AddVoice` drain runs in the audio callback, so `voices.push` must not
/// reallocate there. 64 covers any realistic per-track voice count; a track past
/// it pays one grow on the next add and is then stable again.
const MAX_RESIDENT_VOICES: usize = 64;

const COMMAND_CAPACITY: usize = 64;
const VOICE_POOL_ID: u64 = 0x_0000_0000_0000_DA03;

// ---------------------------------------------------------------------------
// Slot ID — opaque u128 so bevy-tutti stays independent of dawai-types.
// dawai-model converts ClipId ↔ SlotId at the boundary.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(pub u128);

// ---------------------------------------------------------------------------
// Direction — playback direction for a voice. Replaces loose `reverse: bool`
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
// VoiceSource — a voice's audio source, monomorphized. Either the whole source is
// resident in memory (`Memory`) or it streams incrementally from the butler ring
// (`Disk`).
//
// DESIGN INVARIANT: the two variants differ ONLY in the *essential* per-sample
// read — `Memory` indexes an `Arc<Wave>`; `Disk` pops the butler-fed ring,
// emitting silence while `is_seeking()` and crossfading on refill. Every *cold*
// control op is a `match` on this enum — see `apply_gain` / `apply_speed` /
// `apply_direction` / `apply_placement`. A `ClipReader` trait used to sit over
// the pair, but half its methods no-opped on one side or the other, which hid
// the divergence instead of removing it (streaming clamped speed, in-memory did
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
    Memory(MemorySource),
    Disk(DiskVoice),
}

// Hand-rolled: both variants wrap non-`Debug`-deriving units (`MemorySource` /
// `DiskVoice`), each with its own hand-rolled summary Debug.
impl std::fmt::Debug for VoiceSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory(s) => f.debug_tuple("Memory").field(s).finish(),
            Self::Disk(s) => f.debug_tuple("Disk").field(s).finish(),
        }
    }
}

impl Clone for VoiceSource {
    fn clone(&self) -> Self {
        match self {
            Self::Memory(s) => Self::Memory(s.clone()),
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
            Self::Memory(s) => s,
            Self::Disk(s) => s,
        }
    }

    /// Set the output gain. Both tiers store a linear multiplier applied after
    /// the source read, so this is genuinely one operation.
    #[inline]
    fn apply_gain(&mut self, gain: Amplitude) {
        match self {
            Self::Memory(s) => s.set_gain(gain),
            Self::Disk(s) => s.set_gain(gain),
        }
    }

    /// Set varispeed.
    ///
    /// The two tiers store it differently — a unit-local field in memory, an
    /// atomic shared with the butler and every clone on disk — which is exactly
    /// why the bound now lives in [`PlaybackRate`] rather than in one of these
    /// arms.
    #[inline]
    fn apply_speed(&mut self, speed: PlaybackRate) {
        match self {
            Self::Memory(s) => s.set_speed(speed),
            Self::Disk(s) => s.set_speed(speed),
        }
    }

    /// Set playback direction.
    ///
    /// In-memory direction lives on the slot's `Playback`, not inside the unit —
    /// the reversed index is applied at read time — so only the streaming tier
    /// has source-side state to update. The caller writes `play.direction`
    /// either way.
    #[inline]
    fn apply_direction(&mut self, direction: Direction) {
        match self {
            Self::Memory(_) => {}
            Self::Disk(s) => s.set_direction(direction),
        }
    }

    /// Update the timeline placement window.
    #[inline]
    fn apply_placement(&mut self, start_beat: Beat, duration: Option<BeatDuration>) {
        match self {
            Self::Memory(s) => s.set_placement(start_beat, duration),
            Self::Disk(s) => s.set_placement(start_beat, duration),
        }
    }
}

// ---------------------------------------------------------------------------
// Playback — the control-INTENT record for one voice. It says *what* the voice
// should do (gain, speed, direction, loop, timeline placement, stretch, pitch);
// each [`VoiceSource`] APPLIES it its own way (the in-memory `MemorySource` stores
// the state on its resident DSP; the streaming reader forwards to the butler's
// shared `RtState`). The apply fan-out is the `VoiceSource` match — Playback is
// the description, not the applied state.
//
// `Default` is hand-written: the newtypes default to zero, so a derived default
// would ship silent (`gain = 0`) and frozen (`speed = 0` / `stretch = 0`).
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Playback {
    pub gain: Amplitude,
    pub speed: PlaybackRate,
    pub direction: Direction,
    pub loop_: LoopSetting,
    pub placement: Option<TransportPlacement>,
    /// Time-stretch factor (1.0 = no stretch). Absorbed here so the placement,
    /// stretch, and pitch intent live in one record rather than in a sidecar.
    pub stretch: StretchFactor,
    /// Pitch shift in cents (0.0 = no shift).
    pub pitch: Cents,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            gain: Amplitude::new(1.0),
            speed: PlaybackRate::UNITY,
            direction: Direction::Forward,
            loop_: LoopSetting::Off,
            placement: None,
            stretch: StretchFactor::UNITY,
            pitch: Cents::new(0.0),
        }
    }
}

/// The voice's control fields as decoded from ECS at promote time — the flat
/// input to [`Playback::from_pending`]. A voice carries a single `gain` scalar
/// that dawai maps to BOTH the voice gain and the read speed (the historical
/// `playback_rate`), plus the loop range, reverse, and stretch/pitch intent.
/// Grouped so both the memory promote and the Disk poll build a `Playback` from
/// one shape.
#[derive(Debug, Clone, Default)]
pub struct PendingPlayback {
    pub gain: Amplitude,
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
    /// Build the control-intent record for a freshly-promoted voice from its
    /// decoded control fields. This is the single place the pending voice's
    /// gain / loop / reverse / stretch DATA becomes a [`Playback`] — replacing
    /// the old pre-send poking of the `MemorySource` (`set_gain`, the
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
// Voice — one voice's playback state, and what the reader STORES per slot. Its
// `source` is either an in-memory `MemorySource` or a streaming
// `DiskVoice` (the [`VoiceSource`] enum); `play` is the [`Playback`]
// control-intent record; `channel_index` is the butler channel for a `Disk`
// source (`None` for `Memory`), kept on the Voice because the butler loop routing
// needs it.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Voice {
    pub source: VoiceSource,
    pub play: Playback,
    /// Butler channel index for a `Disk` source; `None` for `Memory`. The reader
    /// drain forwards streaming loop ops (`SetStreamLoop` / `ClearStreamLoop`)
    /// to this channel via the typed [`Commands`] handle — loop is butler-owned
    /// (it reads a fadein head off disk + mutates `plan.link.loop_config`,
    /// neither reachable from the reader), so the forward is the honest path.
    /// Meaningless for `Memory` (loop is primed directly on the `MemorySource`).
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
    /// - the source's own read clock. The `Memory` [`MemorySource`] reads its
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
        // Rebind the source's own read clock. Only the `Memory` `MemorySource`
        // exposes a whole-transport swap (`replace_transport`); it is the only
        // source a standalone offline `VoiceNode` ever wraps (resynth /
        // region-render populate build memory voices), so this is the path that
        // matters for the offline render.
        if let VoiceSource::Memory(sampler) = &mut self.source {
            sampler.replace_transport(transport);
        }
    }
}

// ---------------------------------------------------------------------------
// VoiceSlot — a `Voice` plus the resident time-stretch DSP processor. The
// processor is the DSP object (not config), so it stays on the slot alongside
// the engine sample rate; the control intent lives in `voice.play`.
// ---------------------------------------------------------------------------

struct VoiceSlot {
    id: SlotId,
    voice: Voice,
    /// The time-stretch processor is **always resident**: it is built once (one
    /// phase-vocoder construction + two `RtScratch` buffers *per channel*) when
    /// the slot is created, and thereafter the audio thread only flips the
    /// lock-free `stretch_factor` / `pitch_cents` atomics inside it. It owns NO
    /// copy of the audio source: it is a pure frame-in → frame-out filter. At
    /// tick time, the `needs_stretch()` gate (mirrored from those atomics into
    /// `voice.play.stretch` / `voice.play.pitch`) chooses whether to tick the
    /// single source and route its frame through this filter, or read the source
    /// directly.
    ///
    /// **"Built once" is not the same as "built off the audio thread", and this
    /// distinction used to be blurred here.** Per-buffer *updates* are genuinely
    /// allocation-free — [`VoiceCommand::UpdateStretch`] only sets atomics. But
    /// the construction itself happens in `insert_voice`, reached from
    /// [`VoiceCommand::AddVoice`], which `drain_commands` pulls from `tick` /
    /// `process`. So adding a voice mid-playback DOES build the vocoders in the
    /// callback, and at width `n` that is `n` FFT setups plus `2n` scratch
    /// allocations rather than the stereo pair the old wording implied.
    ///
    /// Pre-existing, and not made reachable by the width work — but the cost now
    /// scales with channel count, so it is worth stating plainly instead of
    /// leaving the reader to infer safety.
    ///
    /// Structural invariant: `voice.play.stretch` / `voice.play.pitch` cannot
    /// drift from the processor's atomics — every mutation goes through
    /// [`VoiceSlot::set_stretch`], which writes both in one step.
    stretch: Option<stretch::Unit>,
    /// Width the stretch unit must be built at, remembered so a later
    /// materialisation matches the reader rather than defaulting.
    channels: usize,
    sample_rate: f64,
}

impl VoiceSlot {
    /// Build a slot. Width is explicit at every call site — there is no
    /// stereo-defaulting `new`, because both callers (the reader's drain and
    /// `VoiceNode`) know their own width and a default here would silently
    /// mismatch it.
    ///
    /// The stretch unit is **not** built here. It is `None` until the slot
    /// actually needs it, for two reasons:
    ///
    /// - Most voices never stretch. A `stretch::Unit` is one FFT setup plus two
    ///   `RtScratch` buffers *per channel* (~120 KB × N), so building one for
    ///   every voice spent that on the common case for nothing.
    /// - This constructor is reachable from the audio thread.
    ///   `VoiceCommand::AddVoice` is drained by `drain_commands`, which runs from
    ///   `tick`/`process`, so eager construction meant adding a voice mid-playback
    ///   allocated in the callback.
    ///
    /// A slot that arrives already needing stretch (non-unity `play.stretch` /
    /// `play.pitch`) is given its unit by the SENDER via
    /// [`VoiceSlot::materialize_stretch`], on the control thread, before the
    /// command is queued — see [`VoiceCommand::AddVoice`].
    fn with_channels(id: SlotId, voice: Voice, sample_rate: f64, channels: usize) -> Self {
        let channels = channels.max(1);
        Self {
            id,
            voice,
            stretch: None,
            channels,
            sample_rate,
        }
    }

    /// Whether the slot's control intent asks for stretching. Independent of
    /// whether a unit exists — [`materialize_stretch`](Self::materialize_stretch)
    /// uses this to decide whether to build one.
    fn needs_stretch(&self) -> bool {
        stretch_wanted(&self.voice.play)
    }

    /// Drop every sample of audio this slot is holding mid-flight.
    ///
    /// Called from two places, which is why it is one function: `AudioUnit::reset`
    /// (a graph-level reset) and the per-block seek check (a transport
    /// discontinuity). They must clear exactly the same state — a seek that
    /// flushed less than a reset would leave a subset of the stale audio behind,
    /// which sounds like an intermittent artifact rather than a bug.
    ///
    /// The stretch filter is the state that matters. A placed source re-derives
    /// its position from the playhead every block, so it is correct the instant
    /// the playhead moves; the vocoder's FIFOs and per-bin phase accumulators are
    /// not, and nothing else clears them.
    ///
    /// RT-safe: `Vocoder::reset` is buffer fills and cursor resets, no allocation.
    fn flush_playhead_state(&mut self) {
        self.voice.source.as_audio_unit_mut().reset();
        if let Some(unit) = &mut self.stretch {
            unit.reset();
        }
    }

    /// Update the stretch factors — the only entry point for mutating them.
    /// Lock-free: mirrors the values into `voice.play` (read by the
    /// [`needs_stretch`](Self::needs_stretch) gate) and flips the resident
    /// processor's atomics if one exists.
    ///
    /// **Allocation-free**, so it is safe on the audio-thread command drain.
    /// Turning stretch ON when no unit is resident does NOT build one here — the
    /// sender materialises it before queueing (see
    /// [`VoiceCommand::UpdateStretch`]). Until it arrives the slot reads dry,
    /// which is why the hot paths gate on `needs_stretch() && stretch.is_some()`
    /// rather than on the intent alone.
    fn set_stretch(&mut self, stretch_factor: StretchFactor, pitch_cents: Cents) {
        self.voice.play.stretch = stretch_factor;
        self.voice.play.pitch = pitch_cents;
        if let Some(unit) = &self.stretch {
            unit.set_stretch_factor(stretch_factor);
            unit.set_pitch_cents(pitch_cents);
        }
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
        // Borrow the source and the filter as DISJOINT fields: `active_stretch`
        // would hold `&mut self` across the source read otherwise.
        let stretching = self.needs_stretch() && self.stretch.is_some();
        if stretching {
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
            if let Some(unit) = &mut self.stretch {
                unit.tick(&raw[..n], out);
            }
        } else {
            match &mut self.voice.source {
                VoiceSource::Memory(sampler) => match sampler.transport_sample_position() {
                    Some(pos) => read_clip_sample_into(sampler, direction, pos, gain, out),
                    None => out.fill(0.0),
                },
                VoiceSource::Disk(reader) => {
                    // The `DiskVoice` owns its placement gate: it
                    // emits silence outside the voice window and pulls the
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

        // Route through the filter only when the intent asks for it AND a unit
        // is resident; see `active_stretch` for why a missing unit reads dry
        // rather than silent. Destructured so the source and the filter are
        // disjoint borrows.
        let stretching = self.needs_stretch() && self.stretch.is_some();
        let stretch = &mut self.stretch;
        if stretching {
            // Per-sample: read the SINGLE source frame (same per-variant read
            // as the else-branch — the enum still owns the read), then feed
            // it through the stretch filter. The filter's in and out cannot
            // alias, hence the second stack frame.
            let mut raw = [0.0f32; MAX_SAMPLER_CHANNELS];
            let Some(unit) = stretch.as_mut() else {
                return;
            };
            match &mut self.voice.source {
                VoiceSource::Memory(sampler) => {
                    let Some(start_pos) = sampler.transport_sample_position() else {
                        return;
                    };
                    let advance = (sampler.speed().get() * sampler.src_ratio().get()) as f64;
                    for i in 0..size {
                        let pos = start_pos + i as f64 * advance;
                        read_clip_sample_into(sampler, direction, pos, gain, &mut raw[..n]);
                        frame[..n].fill(0.0);
                        unit.tick(&raw[..n], &mut frame[..n]);
                        mix_in!(frame, i);
                    }
                }
                VoiceSource::Disk(reader) => {
                    for i in 0..size {
                        raw[..n].fill(0.0);
                        reader.tick(&[], &mut raw[..n]);
                        frame[..n].fill(0.0);
                        unit.tick(&raw[..n], &mut frame[..n]);
                        mix_in!(frame, i);
                    }
                }
            }
        } else {
            match &mut self.voice.source {
                VoiceSource::Memory(sampler) => {
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

/// Read the reversed-or-forward source sample from an in-memory `MemorySource`, scaled
/// by the voice's `gain`. Free function so both the mixer/`VoiceNode` read and
/// the stretch feed share it.
///
/// The `MemorySource` is a PURE producer here: it reads the raw interpolated
/// frame via `get_sample_raw` (no gain), and gain is applied ONCE at this
/// Voice/Playback level from `gain` (mirrored from `voice.play.gain`). This
/// matches the streaming tier, where gain lives in the source's shared state,
/// and keeps a single, well-defined gain application point per tier.
#[inline]
fn read_clip_sample_into(
    sampler: &MemorySource,
    direction: Direction,
    pos: f64,
    gain: Amplitude,
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
/// still owns the read. `gain` scales the in-memory read at the Voice level (see
/// [`read_clip_sample_into`]); the streaming reader applies its own gain
/// internally. Writes every element of `out`. Used to feed the stretch filter
/// (which owns no source) on the hot path.
#[inline]
fn read_source_frame_into(
    source: &mut VoiceSource,
    direction: Direction,
    gain: Amplitude,
    out: &mut [f32],
) {
    match source {
        VoiceSource::Memory(sampler) => match sampler.transport_sample_position() {
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
pub enum VoiceCommand {
    /// Add a voice by handing the reader a fully-formed [`Voice`]: a
    /// [`VoiceSource`] (in-memory `MemorySource` or streaming `DiskVoice`)
    /// plus its [`Playback`] control-intent record. The drain builds the
    /// [`VoiceSlot`] and applies the full `Playback` per-tier by reusing the same
    /// cold-path appliers the update commands use (`VoiceSource::apply_*` +
    /// `apply_loop`) — no allocation or I/O on the hot path, since the
    /// source (memory `MemorySource` or butler-registered `DiskVoice`) is
    /// built entirely on the ECS/butler side before the send.
    ///
    /// This one command replaces the former split `Add` (in-memory) /
    /// `AddStreaming` (disk) pair: the tier now rides inside `source` and the
    /// butler channel inside `Playback`-adjacent `Voice::channel_index` (carried
    /// on the `Disk` construction), so dawai speaks ONE add command for both
    /// tiers.
    AddVoice {
        id: SlotId,
        /// Boxed: a [`Voice`] carries a whole `MemorySource`/`DiskVoice`,
        /// far larger than the other command variants — boxing keeps the bounded
        /// command channel's per-slot footprint small. Cold path (drained off the
        /// per-sample loop), so the indirection costs nothing audible.
        voice: Box<Voice>,
        /// A stretch filter built by the SENDER, on the control thread, when
        /// `voice.play` asks for stretching.
        ///
        /// The drain runs inside `tick`/`process`, so building this there would
        /// allocate in the audio callback (an FFT setup plus two `RtScratch`
        /// buffers per channel). [`VoicePoolHandle::send`] fills it in
        /// before the command is queued; the drain only moves it into the slot.
        ///
        /// `None` when the voice does not stretch, which is the common case and
        /// costs nothing.
        stretch: Option<Box<stretch::Unit>>,
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
        gain: Amplitude,
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
impl std::fmt::Debug for VoiceCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AddVoice { id, voice, stretch } => f
                .debug_struct("AddVoice")
                .field("id", id)
                .field("voice", voice)
                .field("stretch_prebuilt", &stretch.is_some())
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
// Handle — held by ECS systems, sends commands to the audio-thread unit.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct VoicePoolHandle {
    tx: Sender<VoiceCommand>,
    /// The reader's output width, copied at construction (it is fixed for the
    /// reader's lifetime). Lets [`send`](Self::send) build a stretch filter at
    /// the right width on the CONTROL thread — see
    /// [`VoiceCommand::AddVoice::stretch`].
    channels: usize,
    /// The reader's sample rate at construction, for the same reason.
    sample_rate: f64,
}

impl VoicePoolHandle {
    /// Queue a command, doing any allocation it implies **here**, on the calling
    /// (control) thread.
    ///
    /// This is the one chokepoint every command passes through, which makes it
    /// the right place to keep the audio thread clean: `drain_commands` runs
    /// from `tick`/`process`, so anything expensive left for the drain is an
    /// allocation in the callback. Today that means materialising the stretch
    /// filter for an `AddVoice` that needs one.
    pub fn send(&self, cmd: VoiceCommand) {
        let cmd = self.prepare(cmd);
        match self.tx.try_send(cmd) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                #[cfg(feature = "bevy")]
                bevy_log::warn!("VoicePool command queue full, dropping command");
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }

    /// Move control-thread work out of the drain. Runs on the caller's thread.
    fn prepare(&self, cmd: VoiceCommand) -> VoiceCommand {
        match cmd {
            VoiceCommand::AddVoice {
                id,
                voice,
                stretch: None,
            } if stretch_wanted(&voice.play) => {
                let unit = stretch::Unit::with_channels(self.sample_rate, self.channels);
                unit.set_stretch_factor(voice.play.stretch);
                unit.set_pitch_cents(voice.play.pitch);
                VoiceCommand::AddVoice {
                    id,
                    voice,
                    stretch: Some(Box::new(unit)),
                }
            }
            other => other,
        }
    }
}

/// Whether a [`Playback`] record asks for stretching. Shared by the sender (to
/// decide whether to build a filter) and [`VoiceSlot::needs_stretch`] (to decide
/// whether to route through one), so the two cannot disagree about what
/// "stretching" means.
#[inline]
fn stretch_wanted(play: &Playback) -> bool {
    (play.stretch.get() - 1.0).abs() > 0.001 || play.pitch.get().abs() > 0.5
}

// ---------------------------------------------------------------------------
// ECS components — live on the track entity.
// ---------------------------------------------------------------------------

#[cfg(feature = "bevy")]
#[derive(Component, Debug)]
pub struct VoicePoolRef(pub VoicePoolHandle);

#[cfg(feature = "bevy")]
#[derive(Component, Debug, Clone, Copy)]
pub struct VoicePoolNode(pub tutti_core::NodeId);

// ---------------------------------------------------------------------------
// VoicePool — the AudioUnit.
// ---------------------------------------------------------------------------

pub struct VoicePool {
    voices: Vec<VoiceSlot>,
    rx: Receiver<VoiceCommand>,
    sample_rate: f64,
    transport: Option<Arc<dyn Timeline>>,
    /// Typed butler write handle. `Some` on the live path (threaded in from the
    /// [`DiskStreamer`](crate::DiskStreamer)); `None` for tests / detached / offline
    /// readers with no live butler. Used by the drain to forward *streaming*
    /// loop ops (`Command::Loop`) — loop is butler-owned and not reachable from
    /// the reader itself. Cloning it is cheap (a `Sender` + an `Arc` map).
    butler: Option<Commands>,

    /// Output width — this node's `outputs()`, fixed at construction.
    ///
    /// Declared rather than inferred from the voices it holds: this unit is built
    /// on track creation, *before* any voice exists, and `Net` edges are wired
    /// against `outputs()`. A width that followed its contents would re-arity a
    /// live graph node the moment a voice landed.
    channels: usize,

    /// Detects transport discontinuities, so buffered audio can be flushed on a
    /// seek. `None` when there is no transport to watch (free-running / detached).
    ///
    /// **One cursor for the whole reader, not one per slot.** Every slot reads the
    /// same transport, so N cursors would be N redundant atomic loads per block
    /// and N chances to disagree about whether the playhead moved — and a
    /// disagreement would flush some slots and not others, which is worse than
    /// flushing none.
    ///
    /// Held here rather than derived per block because the detection *is* the
    /// state: a jump is a fact about two consecutive readings, so something has to
    /// remember the previous one. [`BeatCursor`] is that memory, and it already
    /// handles the paused case (a seek made while stopped is reconciled rather
    /// than reported) and clones by sharing, so fundsp's clone-on-commit does not
    /// restart playback.
    cursor: Option<BeatCursor>,
}

// Hand-rolled: `voices` holds non-`Debug` `VoiceSlot`s (each wraps a sampler +
// stretch DSP) and `transport` is an `Arc<dyn Timeline>`. Print the slot
// count + scalars rather than the slot internals.
impl std::fmt::Debug for VoicePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoicePool")
            .field("voices", &self.voices.len())
            .field("sample_rate", &self.sample_rate)
            .field("has_transport", &self.transport.is_some())
            .field("has_butler", &self.butler.is_some())
            .finish_non_exhaustive()
    }
}

impl VoicePool {
    /// Build a unit from an already-created command receiver + optional
    /// transport. Shared field-literal source for `new` / `with_transport` /
    /// `detached`.
    fn from_parts(
        rx: Receiver<VoiceCommand>,
        transport: Option<Arc<dyn Timeline>>,
        butler: Option<Commands>,
    ) -> Self {
        Self::from_parts_with_channels(rx, transport, butler, 2)
    }

    fn from_parts_with_channels(
        rx: Receiver<VoiceCommand>,
        transport: Option<Arc<dyn Timeline>>,
        butler: Option<Commands>,
        channels: usize,
    ) -> Self {
        Self {
            // Reserved, not empty: `AddVoice` is drained inside `tick`/`process`,
            // so a `push` that grows this vector is a reallocation in the audio
            // callback. `MAX_RESIDENT_VOICES` is the point past which a track
            // stops being allocation-free; beyond it the push still works, it
            // just costs one grow.
            voices: Vec::with_capacity(MAX_RESIDENT_VOICES),
            rx,
            sample_rate: 44100.0,
            cursor: transport
                .as_ref()
                .map(|t| BeatCursor::new(Arc::clone(t), 44100.0)),
            transport,
            butler,
            channels: channels.max(1),
        }
    }

    /// Output width — this node's `outputs()`.
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn new() -> (Self, VoicePoolHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let handle = VoicePoolHandle {
            tx,
            channels: 2,
            sample_rate: 44100.0,
        };
        (Self::from_parts(rx, None, None), handle)
    }

    pub fn with_transport(
        transport: Arc<dyn Timeline>,
        butler: Option<Commands>,
    ) -> (Self, VoicePoolHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let handle = VoicePoolHandle {
            tx,
            channels: 2,
            sample_rate: 44100.0,
        };
        (Self::from_parts(rx, Some(transport), butler), handle)
    }

    /// As [`with_transport`](Self::with_transport), at an explicit output width.
    ///
    /// Each slot's stretch unit is built at this width too, so a wide voice is
    /// not truncated on the stretch path.
    pub fn with_channels(
        transport: Option<Arc<dyn Timeline>>,
        butler: Option<Commands>,
        channels: usize,
    ) -> (Self, VoicePoolHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let unit = Self::from_parts_with_channels(rx, transport, butler, channels);
        // The handle mirrors the reader's width/rate so `send` can build a
        // stretch filter that matches it, on the control thread.
        let handle = VoicePoolHandle {
            tx,
            channels: unit.channels,
            sample_rate: unit.sample_rate,
        };
        (unit, handle)
    }

    /// Flush every slot's buffered audio if the playhead moved discontinuously.
    ///
    /// Called once per block from both `tick` and `process`, right after the
    /// command drain. The check is one [`BeatCursor::advance`] — two atomic loads
    /// and a comparison — and on the overwhelmingly common continuous block it
    /// does nothing else.
    ///
    /// **Why anything is needed at all:** [`Timeline`] is poll-only. It reports
    /// where the playhead *is*, never that it moved discontinuously, and it
    /// cannot — "since when" differs per observer, so a shared `last_beat` on the
    /// transport would be overwritten by whichever reader polled last. Each
    /// observer keeps its own cursor; this is the sampler's.
    ///
    /// **Either direction, not just backward.** A backward jump is the obvious
    /// case, but a forward scrub is equally destructive to a FIFO: the buffer
    /// keeps draining the old region's material over the new one. Hence
    /// [`BeatWindowSync::is_discontinuous`] rather than a `Rewound` match — the
    /// distinction the MIDI consumers need (their sorted-list cursors
    /// self-correct forward) is not one buffered audio can afford.
    #[inline]
    fn flush_on_seek(&mut self, block_size: usize) {
        let Some(cursor) = &self.cursor else { return };
        let Some((_, sync)) = cursor.advance(block_size) else {
            return;
        };
        if !sync.is_discontinuous() {
            return;
        }
        for slot in &mut self.voices {
            slot.flush_playhead_state();
        }
    }

    /// Number of voice slots currently materialised (drained from the command
    /// queue). Diagnostic / test helper.
    pub fn voice_count(&self) -> usize {
        self.voices.len()
    }

    /// The control intent recorded for a slot. Test helper: lets a test assert
    /// that a dropped command did NOT leave a `Playback` claiming it applied.
    #[cfg(test)]
    fn playback_of(&self, id: SlotId) -> Option<&Playback> {
        self.voices
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
    /// carry live voice state. Rather than clone-then-sever, the render Prepare
    /// step *replaces* each reader node with one of these: born empty, born
    /// channel-less, so it shares zero mutable state with the live graph at any
    /// instant. Voices are then rebuilt from ECS in the Populate step via
    /// [`Self::insert_voice`].
    pub fn detached(transport: Arc<dyn Timeline>) -> Self {
        let (_tx, rx) = bounded(0);
        // No butler: the offline render never forwards streaming loop ops (it
        // rebuilds in-memory voices from ECS), so a `None` handle is correct here.
        Self::from_parts(rx, Some(transport), None)
    }

    /// Re-point the reader at a new transport. The offline region render calls
    /// this after [`isolate`](AudioUnit::isolate) has emptied the reader, so it
    /// only needs to seat the render's transport; any voices inserted afterward
    /// (via [`insert_voice`](Self::insert_voice)) are built against it. Mirrors
    /// [`VoiceNode::replace_transport`] so both transport-aware nodes rebind the
    /// same way in the render's isolation pass.
    pub fn replace_transport(&mut self, transport: Arc<dyn Timeline>) {
        for slot in &mut self.voices {
            slot.voice.replace_transport(transport.clone());
        }
        self.transport = Some(transport);
    }

    /// Drop every voice slot.
    ///
    /// The offline render clones the staged net and then rebuilds each voice
    /// fresh from ECS + the wave cache; clearing the inherited slots first
    /// keeps the cloned reader from carrying any state tied to the live graph.
    pub fn clear_voices(&mut self) {
        self.voices.clear();
    }

    fn slot_mut(&mut self, id: SlotId) -> Option<&mut VoiceSlot> {
        self.voices.iter_mut().find(|s| s.id == id)
    }

    /// Apply a loop setting to a slot, routing by tier.
    ///
    /// - **In-memory**: primes / clears the loop range on the `MemorySource` in-unit
    ///   (`MemorySource::set_loop_setting`).
    /// - **Streaming**: loop is butler-owned — `SetStreamLoop` reads a loop-start
    ///   fadein head off disk and mutates `plan.link.loop_config`, neither
    ///   reachable from the reader — so the reader FORWARDS to the butler via the
    ///   typed [`Commands`] handle (`Command::Loop`, which maps `On`→
    ///   `SetStreamLoop` / `Off`→`ClearStreamLoop`). This is the same command
    ///   dawai-model used to send itself; it now originates here so dawai speaks
    ///   one unified `VoiceCommand` for both tiers.
    ///
    /// RT-safe: this runs on the COLD command drain (top of `tick`/`process`,
    /// before the per-sample loop), so the channel send is fine — it never
    /// touches the per-sample hot path.
    fn apply_loop(&mut self, id: SlotId, setting: LoopSetting) {
        let Some(slot) = self.voices.iter_mut().find(|s| s.id == id) else {
            return;
        };
        match &mut slot.voice.source {
            VoiceSource::Memory(sampler) => {
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
    /// funnels through. Public so a voice-aware caller (the offline region
    /// render's `Populate` step) can hand the reader a `Voice` it built from
    /// ECS DATA — gain / loop / direction / stretch / pitch carried on
    /// `voice.play` — instead of pre-poking a `MemorySource` before send.
    ///
    /// The `Playback` is control-INTENT; each tier applies it its own way. Rather
    /// than duplicate the tier fork, we replay the exact cold-path appliers the
    /// `Update*` commands use: `VoiceSource::apply_*` (per-tier match — in-memory
    /// stores on the `MemorySource`, streaming forwards to the shared `RtState`)
    /// for gain / speed / direction, and `apply_loop` for loop (in-memory
    /// primes the range in-unit, streaming forwards `Command::Loop` to the
    /// butler). Stretch/pitch are primed by `VoiceSlot::new` from `play`. Runs on
    /// the COLD command drain, so the loop's butler send is RT-safe.
    ///
    /// [`AddVoice`]: VoiceCommand::AddVoice
    /// As [`insert_voice`](Self::insert_voice), taking a stretch filter the
    /// caller already built.
    ///
    /// This is the audio-thread-safe form: the drain uses it so the callback
    /// only MOVES a filter rather than constructing one. `None` leaves the slot
    /// without a filter, which is correct both for a voice that does not stretch
    /// and (transiently) for one whose filter has not arrived yet: the hot paths
    /// then read the source dry rather than silencing it.
    pub fn insert_voice_with_stretch(
        &mut self,
        id: SlotId,
        voice: Voice,
        stretch: Option<stretch::Unit>,
    ) {
        self.insert_voice_inner(id, voice, stretch);
    }

    /// Insert a voice, building the stretch filter here if one is needed.
    ///
    /// **Allocates when the voice stretches** — control-thread callers only.
    /// The audio-thread drain goes through
    /// [`insert_voice_with_stretch`](Self::insert_voice_with_stretch) instead.
    pub fn insert_voice(&mut self, id: SlotId, voice: Voice) {
        let stretch = stretch_wanted(&voice.play).then(|| {
            let unit = stretch::Unit::with_channels(self.sample_rate, self.channels);
            unit.set_stretch_factor(voice.play.stretch);
            unit.set_pitch_cents(voice.play.pitch);
            unit
        });
        self.insert_voice_inner(id, voice, stretch);
    }

    fn insert_voice_inner(&mut self, id: SlotId, voice: Voice, stretch: Option<stretch::Unit>) {
        self.voices.retain(|s| s.id != id);
        // Split the loop out: `apply_loop` needs the slot present to look it up,
        // and `Playback` moves into the `Voice`. Take the rest by copy first.
        let loop_ = voice.play.loop_.clone();
        let gain = voice.play.gain;
        let speed = voice.play.speed;
        let direction = voice.play.direction;
        // `VoiceSlot::with_channels` primes the resident stretch unit from
        // `voice.play` (stretch/pitch) — the one heavy step, done here off the
        // hot path. It is built at THIS READER's width: a slot narrower than the
        // reader would truncate on the stretch path only, which no stereo test
        // can observe.
        let mut slot = VoiceSlot::with_channels(id, voice, self.sample_rate, self.channels);
        slot.stretch = stretch;
        self.voices.push(slot);

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
                VoiceCommand::AddVoice { id, voice, stretch } => {
                    // The filter arrives prebuilt from `send` (control thread);
                    // this only moves it into the slot.
                    self.insert_voice_with_stretch(id, *voice, stretch.map(|b| *b));
                }
                VoiceCommand::Remove(id) => {
                    self.voices.retain(|s| s.id != id);
                }
                VoiceCommand::ReplaceWave { id, wave } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // Match the tier explicitly rather than calling through
                        // a shared setter whose streaming half was a silent
                        // no-op: a `ReplaceWave` aimed at a disk voice did
                        // nothing and said nothing. Swapping a streaming
                        // source means re-registering the butler stream on a
                        // different file, which is a control-thread op issued
                        // from dawai-model as a fresh `AddVoice`.
                        match &mut slot.voice.source {
                            VoiceSource::Memory(sampler) => sampler.set_wave(wave),
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
                VoiceCommand::UpdatePlacement {
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
                VoiceCommand::UpdateGain { id, gain } => {
                    if let Some(slot) = self.slot_mut(id) {
                        slot.voice.source.apply_gain(gain);
                        slot.voice.play.gain = gain;
                    }
                }
                VoiceCommand::UpdateSpeed { id, speed } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // Unified across tiers: both backends carry speed in-unit.
                        // In-memory stores it on the `MemorySource`; streaming forwards
                        // to the shared `RtState` (the exact speed effect of the
                        // butler's `SetVarispeed`, reachable from the reader). The
                        // `apply_speed` covers both — dawai no longer
                        // forks streaming speed onto a separate butler command.
                        slot.voice.source.apply_speed(speed);
                        slot.voice.play.speed = speed;
                    }
                }
                VoiceCommand::UpdateLoop {
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
                VoiceCommand::ClearLoop(id) => {
                    self.apply_loop(id, LoopSetting::Off);
                }
                VoiceCommand::UpdateReverse { id, direction } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // In-memory: `voice.play.direction` drives the reversed index
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
                VoiceCommand::UpdateStretch {
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

impl Clone for VoicePool {
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
            voices: self
                .voices
                .iter()
                .map(|s| VoiceSlot {
                    id: s.id,
                    voice: s.voice.clone(),
                    stretch: s.stretch.clone(),
                    channels: s.channels,
                    sample_rate: s.sample_rate,
                }) // Voice (VoiceSource) + resident stretch::Unit clone by value; atomics preserved
                .collect(),
            rx: self.rx.clone(),
            sample_rate: self.sample_rate,
            // Shares the underlying cursor cell, so a clone-on-commit does not
            // read as a discontinuity and restart playback.
            cursor: self.cursor.clone(),
            transport: self.transport.clone(),
            butler: self.butler.clone(),
            channels: self.channels,
        }
    }
}

impl AudioUnit for VoicePool {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        self.channels
    }

    fn reset(&mut self) {
        for slot in &mut self.voices {
            slot.flush_playhead_state();
        }
    }

    /// Sever the live command channel and clear live voice state so an offline
    /// clone can be ticked on a worker thread without stealing commands from —
    /// or sharing voices with — the live reader. Leaves the unit *born empty and
    /// channel-less* (the [`detached`](Self::detached) state), minus the
    /// transport: re-pointing at the render's offline transport is the caller's
    /// separate data-carrying step (via [`replace_transport`](Self::replace_transport)),
    /// per the `isolate` contract.
    fn isolate(&mut self) {
        // A `bounded(0)` receiver whose sender is dropped can never deliver, so
        // the worker's drain sees nothing and steals nothing from the live rx.
        let (_tx, rx) = bounded(0);
        self.rx = rx;
        self.voices.clear();
        // The offline render rebuilds voices from ECS and never forwards
        // streaming loop ops, so it needs no butler handle.
        self.butler = None;
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate.get();
        if let Some(cursor) = &mut self.cursor {
            cursor.set_sample_rate(sample_rate.get());
        }
        for slot in &mut self.voices {
            slot.voice
                .source
                .as_audio_unit_mut()
                .set_sample_rate(sample_rate);
            slot.sample_rate = sample_rate.get();
            if let Some(unit) = &mut slot.stretch {
                unit.set_sample_rate(sample_rate);
            }
        }
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        self.drain_commands();
        // One frame is one "block" here: `tick` is the per-sample entry point, so
        // the forward-jump slack is measured in single samples rather than 64.
        self.flush_on_seek(1);

        let n = self.channels.min(output.len()).min(MAX_SAMPLER_CHANNELS);
        if n == 0 {
            return;
        }
        output[..n].fill(0.0);

        // Each slot reads its ONE voice via the shared
        // `VoiceSlot::tick_frame_into` (the same per-variant `VoiceSource` match
        // a standalone `VoiceNode` uses — factored, not duplicated, and no
        // per-sample dyn), summed channel-wise into the caller's frame.
        let mut frame = [0.0f32; MAX_SAMPLER_CHANNELS];
        for slot in &mut self.voices {
            slot.tick_frame_into(&mut frame[..n]);
            for (c, &s) in frame.iter().enumerate().take(n) {
                output[c] += s;
            }
        }
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        self.drain_commands();
        self.flush_on_seek(size.max(1));

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
        // `VoiceSlot::process_into` (the same per-variant `VoiceSource` match a
        // standalone `VoiceNode` uses).
        for slot in &mut self.voices {
            slot.process_into(size, n, output);
        }
    }

    audio_unit_boilerplate!(id = VOICE_POOL_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        SignalFrame::new(self.channels)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>() + self.voices.len() * std::mem::size_of::<VoiceSlot>()
    }
}

// ---------------------------------------------------------------------------
// VoiceNode — a standalone single-`Voice` graph node (0 inputs, 2 outputs).
//
// The mixer (`VoicePool`) holds a *list* of voices and sums them; a
// `VoiceNode` holds exactly ONE and plays it — a degenerate single-voice
// mixdown. Its `tick`/`process` are the SAME per-voice read the mixer does for
// one slot, shared through [`VoiceSlot::tick_frame`] / [`VoiceSlot::process_into`]
// (no duplication, no per-sample dyn).
//
// This is the standalone-graph-node case: `dawai-spectral`'s resynth adds a bare
// voice node to its net, and `tutti-export`'s region render downcasts these
// nodes to rebind their transport offline. Both rely on a `Voice` being an
// `AudioUnit` in its own right — not only reachable through the mixer — so this
// node keeps that path alive (guarded by a test).
// ---------------------------------------------------------------------------

pub struct VoiceNode {
    slot: VoiceSlot,
    /// Output width — see [`VoicePool`]'s field of the same name.
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
            slot: VoiceSlot::with_channels(SlotId(0), voice, 44100.0, channels),
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

// Hand-rolled: wraps a non-`Debug` `VoiceSlot`. Print the wrapped `Voice`
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
            slot: VoiceSlot {
                id: self.slot.id,
                voice: self.slot.voice.clone(),
                stretch: self.slot.stretch.clone(),
                channels: self.slot.channels,
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
        if let Some(unit) = &mut self.slot.stretch {
            unit.reset();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.slot
            .voice
            .source
            .as_audio_unit_mut()
            .set_sample_rate(sample_rate);
        self.slot.sample_rate = sample_rate.get();
        if let Some(unit) = &mut self.slot.stretch {
            unit.set_sample_rate(sample_rate);
        }
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
    use crate::voice::memory_source::MemorySourceConfig;
    use tutti_core::Bpm;

    use crate::test_transport::MockTransport;

    fn make_wave(samples: usize) -> Arc<Wave> {
        let data: Vec<f32> = (0..samples)
            .map(|i| (i as f32 + 1.0) / samples as f32)
            .collect();
        Arc::new(Wave::from_samples(44100.0, &data))
    }

    /// Send an in-memory voice through the one `AddVoice` path — the test-setup
    /// mirror of the timeline's `promote_pending_clip_waves` emit.
    fn add_ram_clip(handle: &VoicePoolHandle, id: SlotId, sampler: MemorySource) {
        handle.send(VoiceCommand::AddVoice {
            id,
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback::default(),
                channel_index: None,
            }),
            stretch: None,
        });
    }

    /// A transport seek must flush the stretch filter's buffered audio.
    ///
    /// **This is the gate for the seek bug and it FAILS before the fix.**
    ///
    /// `Timeline` is poll-only — `beat()` / `tempo()` / `is_rolling()`, no seek
    /// event — and nothing on any transport-driven path calls
    /// `AudioUnit::reset()`. So when the playhead jumps, the placement gate
    /// re-derives the new position correctly and immediately, while
    /// `stretch::Unit` keeps draining a FIFO primed from *before* the jump: up
    /// to `window * 4` samples per channel, plus per-bin phase accumulators
    /// still tracking the old material.
    ///
    /// The wave is loud in its first half and **exactly silent** in its second,
    /// which is what makes the assertion about the bug rather than about
    /// liveness. Every other stretch test on this path asserts only `!= 0.0` or
    /// `> 1e-6` (`clips_sum_together`, `six_channel_clip_with_stretch_...`), and
    /// a leak at signal level passes all of them — the same blind spot that let
    /// a 60 dB gain error live in the vocoder. Here, parking the playhead in the
    /// silent half means any output above the floor is provably material the
    /// filter should no longer be holding.
    #[test]
    fn a_transport_seek_flushes_stretch_state() {
        const SR: f64 = 44_100.0;
        // Ten seconds, so the playhead has room to run for thousands of blocks
        // inside one half without leaving it.
        const LEN: usize = 441_000;
        // 120 BPM = 2 beats/s, so the wave spans 20 beats and the halves split at
        // beat 10.
        const LOUD_BEAT: f64 = 2.0;
        const SILENT_BEAT: f64 = 12.0;

        // Loud first half at 3 kHz (a real signal — the vocoder needs a changing
        // input to synthesise from), exactly silent second half.
        let data: Vec<f32> = (0..LEN)
            .map(|i| {
                if i < LEN / 2 {
                    0.5 * (std::f32::consts::TAU * 3000.0 * i as f32 / SR as f32).sin()
                } else {
                    0.0
                }
            })
            .collect();
        let wave = Arc::new(Wave::from_samples(SR, &data));

        let transport = MockTransport::rolling(Beat::new(SILENT_BEAT), Bpm::new(120.0));
        let (mut unit, handle) = VoicePool::with_channels(Some(transport.clone()), None, 1);

        let sampler = MemorySource::with_transport(wave, transport.clone(), Beat::new(0.0), None);
        handle.send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Playback::default()
                },
                channel_index: None,
            }),
            // The sender materialises the filter on the control thread, as
            // `VoicePoolHandle::send` does for a voice that arrives
            // already needing one.
            stretch: None,
        });

        let mut out = [0.0f32; 1];
        unit.tick(&[], &mut out);
        assert!(
            unit.voices[0].needs_stretch(),
            "test is vacuous unless the stretch path is live"
        );

        // Advance the playhead one sample per tick, the way a real transport
        // moves — a frozen playhead would make every frame re-derive the same
        // position, so the source would emit a constant and the vocoder would
        // have nothing to synthesise from.
        let drive = |unit: &mut VoicePool, n: usize| {
            let mut peak = 0.0f32;
            let mut out = [0.0f32; 1];
            for _ in 0..n {
                unit.tick(&[], &mut out);
                transport.advance(1, SR);
                peak = peak.max(out[0].abs());
            }
            peak
        };

        // Settle in the silent half: whatever the filter emits here is the floor.
        drive(&mut unit, 8192);

        // Seek backwards into the loud half and prime the filter with it.
        transport.set_beat(Beat::new(LOUD_BEAT));
        let loud = drive(&mut unit, 8192);
        assert!(
            loud > 0.05,
            "the loud half should be audible after seeking into it; peak {loud}"
        );

        // Seek back into the silent half. The source is silent from this playhead
        // on, so anything above the floor is stale.
        transport.set_beat(Beat::new(SILENT_BEAT));
        let after_seek = drive(&mut unit, 2048);

        assert!(
            after_seek < 0.01,
            "stretch filter leaked pre-seek audio across a transport jump: \
             peak {after_seek} (the source is silent at this playhead)"
        );
    }

    /// Continuous playback must NOT flush — the false positive that matters.
    ///
    /// A jump threshold set too tight fires on ordinary blocks, so the vocoder
    /// resets constantly and its output becomes a stutter of ~50 ms fragments.
    /// Nothing else in the suite would see it: every other stretch assertion is
    /// `!= 0.0` or `> 1e-6`, and a stuttering stretcher is still non-zero. So
    /// this asserts *continuity of level* across hundreds of ordinary blocks —
    /// after the pipeline fills, no block may collapse to silence.
    ///
    /// # Currently ignored: it fails on a SEPARATE, pre-existing bug
    ///
    /// It measures 32/256 silent blocks — and so does the same measurement taken
    /// at `stretch::Unit` directly, with no transport and no flushing involved.
    /// So the dropouts are not spurious flushes; the stretcher stutters on its
    /// own.
    ///
    /// The cause is an unbounded backlog in `OverlapAdd`. At stretch `s`, one
    /// frame consumes `hop` input samples and writes `hop * s` output samples,
    /// while `tick` pushes and pops exactly one sample per call. For `s > 1` the
    /// ring therefore grows by `hop * (s - 1)` per frame, without bound — 31,743
    /// samples pending against a `window * 4` = 8,192 capacity when measured — so
    /// the write cursor laps the read cursor and overwrites audio that was never
    /// read. No fixed capacity fixes it; the drain rate has to match, which is a
    /// design question about what `tick` means for a rate-changing filter.
    ///
    /// Un-ignore this together with that fix. Keeping it here, ignored and
    /// explained, is deliberate: the assertion is the right one, and it is the
    /// only thing in the suite that can see either bug.
    #[test]
    #[ignore = "fails on the pre-existing OverlapAdd backlog overflow, not on flushing"]
    fn continuous_playback_does_not_flush_the_stretch_filter() {
        const SR: f64 = 44_100.0;
        const LEN: usize = 441_000;

        // Steady 3 kHz throughout: any dropout is the filter being reset, not the
        // material.
        let data: Vec<f32> = (0..LEN)
            .map(|i| 0.5 * (std::f32::consts::TAU * 3000.0 * i as f32 / SR as f32).sin())
            .collect();
        let wave = Arc::new(Wave::from_samples(SR, &data));

        let transport = MockTransport::rolling(Beat::new(1.0), Bpm::new(120.0));
        let (mut unit, handle) = VoicePool::with_channels(Some(transport.clone()), None, 1);

        let sampler = MemorySource::with_transport(wave, transport.clone(), Beat::new(0.0), None);
        handle.send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Playback::default()
                },
                channel_index: None,
            }),
            stretch: None,
        });

        let mut out = [0.0f32; 1];
        // Fill the vocoder pipeline first: the opening blocks are legitimately
        // quiet while the FIFOs and overlap-add tail prime.
        for _ in 0..16_384 {
            unit.tick(&[], &mut out);
            transport.advance(1, SR);
        }
        assert!(unit.voices[0].needs_stretch(), "stretch path must be live");

        // Steady state: measure per-block peaks and require every one to carry
        // signal. A reset mid-run empties the FIFO, so the following block is
        // silent — which is exactly what this catches.
        let mut quiet_blocks = 0usize;
        for _ in 0..256 {
            let mut peak = 0.0f32;
            for _ in 0..64 {
                unit.tick(&[], &mut out);
                transport.advance(1, SR);
                peak = peak.max(out[0].abs());
            }
            if peak < 0.01 {
                quiet_blocks += 1;
            }
        }
        assert_eq!(
            quiet_blocks, 0,
            "continuous playback flushed the filter: {quiet_blocks}/256 blocks fell silent"
        );
    }

    #[test]
    fn add_and_remove_clips() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        let sampler =
            MemorySource::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(out[0] != 0.0 || out[1] != 0.0, "voice should produce audio");

        handle.send(VoiceCommand::Remove(SlotId(1)));
        unit.tick(&[], &mut out);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn silence_when_transport_stopped() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::stopped(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn clips_sum_together() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        for i in 0..3 {
            let sampler =
                MemorySource::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
            add_ram_clip(&handle, SlotId(i), sampler);
        }

        let mut out_3 = [0.0f32; 2];
        unit.tick(&[], &mut out_3);

        let (mut unit2, handle2) = VoicePool::new();
        let sampler =
            MemorySource::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
        add_ram_clip(&handle2, SlotId(0), sampler);
        let mut out_1 = [0.0f32; 2];
        unit2.tick(&[], &mut out_1);

        let tolerance = 0.001;
        assert!((out_3[0] - out_1[0] * 3.0).abs() < tolerance);
        assert!((out_3[1] - out_1[1] * 3.0).abs() < tolerance);
    }

    #[test]
    fn clone_snapshots_clips() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);

        let mut cloned = unit.clone();
        let mut out_clone = [0.0f32; 2];
        cloned.tick(&[], &mut out_clone);

        assert!(out_clone[0] != 0.0, "cloned unit should have the voice");
    }

    #[test]
    fn insert_voice_is_audible_without_channel() {
        let (mut unit, _handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        unit.insert_voice(
            SlotId(1),
            Voice {
                play: Playback {
                    gain: sampler.gain(),
                    speed: sampler.speed(),
                    loop_: sampler.loop_setting(),
                    direction: Direction::Forward,
                    stretch: StretchFactor::new(1.0),
                    pitch: Cents::new(0.0),
                    placement: None,
                },
                source: VoiceSource::Memory(sampler),
                channel_index: None,
            },
        );

        // No tick/drain needed — the voice is already in the slot list.
        assert_eq!(unit.voice_count(), 1);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(
            out[0] != 0.0 || out[1] != 0.0,
            "inserted voice should produce audio"
        );
    }

    #[test]
    fn detached_reader_has_no_channel_and_is_empty() {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        // A render-only reader is born empty and channel-less: there is no
        // sender for its `rx`, so no command can ever reach it.
        let mut unit = VoicePool::detached(transport.clone());
        assert_eq!(unit.voice_count(), 0, "detached reader starts empty");

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out); // drains its (permanently empty) queue
        assert_eq!(
            unit.voice_count(),
            0,
            "detached reader receives no commands"
        );
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);

        // But voices inserted directly (the Populate path) are audible.
        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        unit.insert_voice(
            SlotId(1),
            Voice {
                play: Playback {
                    gain: sampler.gain(),
                    speed: sampler.speed(),
                    loop_: sampler.loop_setting(),
                    direction: Direction::Forward,
                    stretch: StretchFactor::new(1.0),
                    pitch: Cents::new(0.0),
                    placement: None,
                },
                source: VoiceSource::Memory(sampler),
                channel_index: None,
            },
        );
        unit.tick(&[], &mut out);
        assert!(out[0] != 0.0 || out[1] != 0.0, "inserted voice is audible");
    }

    #[test]
    fn update_gain() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out_before = [0.0f32; 2];
        unit.tick(&[], &mut out_before);

        handle.send(VoiceCommand::UpdateGain {
            id: SlotId(1),
            gain: Amplitude::new(0.5),
        });

        let mut out_after = [0.0f32; 2];
        unit.tick(&[], &mut out_after);

        assert!((out_after[0] - out_before[0] * 0.5).abs() < 0.01);
    }

    #[test]
    fn update_stretch_enables_processor() {
        let (mut unit, handle) = VoicePool::new();
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(4096);

        let sampler = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        add_ram_clip(&handle, SlotId(1), sampler);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(!unit.voices[0].needs_stretch(), "no stretch by default");

        handle.send(VoiceCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(2.0),
            pitch_cents: Cents::new(0.0),
        });
        unit.tick(&[], &mut out);
        assert!(unit.voices[0].needs_stretch(), "stretch should be active");

        handle.send(VoiceCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: StretchFactor::new(1.0),
            pitch_cents: Cents::new(0.0),
        });
        unit.tick(&[], &mut out);
        assert!(
            !unit.voices[0].needs_stretch(),
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
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let wave = make_wave(100);

        // Same voice the mixer would hold for one voice.
        let sampler =
            MemorySource::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
        let voice = Voice {
            source: VoiceSource::Memory(sampler),
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
        let sampler2 = MemorySource::with_transport(wave, transport, Beat::new(0.0), None);
        let (mut mixer, _handle) = VoicePool::new();
        mixer.insert_voice(
            SlotId(1),
            Voice {
                play: Playback {
                    gain: sampler2.gain(),
                    speed: sampler2.speed(),
                    loop_: sampler2.loop_setting(),
                    direction: Direction::Forward,
                    stretch: StretchFactor::new(1.0),
                    pitch: Cents::new(0.0),
                    placement: None,
                },
                source: VoiceSource::Memory(sampler2),
                channel_index: None,
            },
        );
        let mut node2 = VoiceNode::new(Voice {
            source: VoiceSource::Memory(MemorySource::with_transport(
                make_wave(100),
                MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0)),
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
        let t1 = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let sampler = MemorySource::with_transport(wave, t1, Beat::new(0.0), None);
        let mut voice = Voice {
            source: VoiceSource::Memory(sampler),
            play: Playback {
                placement: Some(TransportPlacement {
                    transport: MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0)),
                    start_beat: Beat::new(2.0),
                    duration_beats: None,
                }),
                ..Playback::default()
            },
            channel_index: None,
        };

        let t2 = MockTransport::stopped(Beat::new(1.0), Bpm::new(140.0));
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
        use crate::voice::disk_voice::DiskSource;
        use crate::voice::disk_voice::{DiskVoice, DiskVoiceConfig};

        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let (mut unit, handle) = VoicePool::new();

        // Build a Disk voice with no butler channel.
        let (writer, reader) =
            RegionBuffer::with_capacity(RegionId(1), std::path::PathBuf::new(), 128, 2);
        drop(writer);
        let state = std::sync::Arc::new(RtState::new());
        let inner = DiskSource::new(share_reader(reader), state.clone());
        let clip_reader = DiskVoice::new(
            inner,
            state,
            DiskVoiceConfig {
                placement: TransportPlacement {
                    transport: transport.clone(),
                    start_beat: Beat::new(0.0),
                    duration_beats: None,
                },
                file_sample_rate: 44100.0,
            },
        );

        let id = SlotId(7);
        handle.send(VoiceCommand::AddVoice {
            id,
            voice: Box::new(Voice {
                source: VoiceSource::Disk(clip_reader),
                play: Playback::default(),
                channel_index: None,
            }),
            stretch: None,
        });

        handle.send(VoiceCommand::UpdateLoop {
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
        let (unit, _h) = VoicePool::new();
        assert_eq!(unit.channels(), 2);
        assert_eq!(unit.outputs(), 2);
    }

    /// `route`'s width must track `outputs()` on both nodes, or fundsp mis-plans
    /// their latency — silent except as PDC drift.
    #[test]
    fn route_width_tracks_outputs_on_both_nodes() {
        for w in [1usize, 2, 6, 8] {
            let (mut unit, _h) = VoicePool::with_channels(None, None, w);
            let out = unit.route(&SignalFrame::new(0), 44_100.0);
            assert_eq!(
                out.len(),
                unit.outputs(),
                "reader route/outputs at width {w}"
            );

            let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
            let sampler = MemorySource::with_config(
                indexed_wave(6, 64),
                MemorySourceConfig {
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
                source: VoiceSource::Memory(sampler),
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

    /// A 6-channel voice in a 6-wide reader must reach all six outputs, through
    /// both entry points (`tick` sums into the caller's slice; `process`
    /// accumulates into a planar buffer — different code).
    #[test]
    fn six_channel_clip_reaches_all_six_reader_outputs() {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let (mut unit, _h) = VoicePool::with_channels(Some(transport.clone()), None, 6);
        let sampler = MemorySource::with_config(
            indexed_wave(6, 512),
            MemorySourceConfig {
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
                source: VoiceSource::Memory(sampler),
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
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let (mut unit, _h) = VoicePool::with_channels(Some(transport.clone()), None, 6);
        let sampler = MemorySource::with_config(
            indexed_wave(6, 4096),
            MemorySourceConfig {
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
                source: VoiceSource::Memory(sampler),
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

    /// The SENDER builds the stretch filter, not the drain.
    ///
    /// `VoicePoolHandle::send` runs on the control thread and fills
    /// `AddVoice::stretch` when the voice asks for stretching; `drain_commands`
    /// (which runs inside `tick`/`process`) only moves it in. If construction
    /// ever moves back into the drain, an `AddVoice` sent with `stretch: None`
    /// would still end up with a filter — so this asserts the filter is present
    /// only because the send path put it there.
    #[test]
    fn send_builds_the_stretch_filter_not_the_drain() {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let (mut unit, handle) = VoicePool::with_channels(Some(transport.clone()), None, 6);

        let mk = |stretch: StretchFactor| {
            let sampler = MemorySource::with_config(
                indexed_wave(6, 128),
                MemorySourceConfig {
                    channels: 6,
                    placement: Some(TransportPlacement {
                        transport: transport.clone(),
                        start_beat: Beat::new(0.0),
                        duration_beats: None,
                    }),
                    ..Default::default()
                },
            );
            Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch,
                    ..Default::default()
                },
                channel_index: None,
            }
        };

        // A stretching voice: `send` must attach a filter, at the READER's width.
        // Observe the queued command BEFORE the drain sees it — that is what
        // distinguishes "the sender built it" from "the drain built it", and it
        // is the only observation that can: after draining, a filter is present
        // either way.
        let peeked = {
            let (probe_tx, probe_rx) = bounded(4);
            let probe = VoicePoolHandle {
                tx: probe_tx,
                channels: 6,
                sample_rate: 44100.0,
            };
            probe.send(VoiceCommand::AddVoice {
                id: SlotId(9),
                voice: Box::new(mk(StretchFactor::new(2.0))),
                stretch: None,
            });
            match probe_rx.try_recv() {
                Ok(VoiceCommand::AddVoice { stretch, .. }) => stretch,
                other => panic!("expected a queued AddVoice, got {other:?}"),
            }
        };
        let peeked = peeked.expect(
            "send must attach the filter BEFORE queueing — if this is None the \
             construction has moved back into the audio-thread drain",
        );
        assert_eq!(
            peeked.channels(),
            6,
            "the sender must build at the reader's width"
        );

        handle.send(VoiceCommand::AddVoice {
            id: SlotId(1),
            voice: Box::new(mk(StretchFactor::new(2.0))),
            stretch: None,
        });
        // A non-stretching voice: no filter, because none is needed.
        handle.send(VoiceCommand::AddVoice {
            id: SlotId(2),
            voice: Box::new(mk(StretchFactor::new(1.0))),
            stretch: None,
        });

        let mut out = [0.0f32; 6];
        unit.tick(&[], &mut out); // drains

        let stretching = unit.voices.iter().find(|s| s.id == SlotId(1)).unwrap();
        let plain = unit.voices.iter().find(|s| s.id == SlotId(2)).unwrap();

        let filter = stretching
            .stretch
            .as_ref()
            .expect("send must have built a filter for the stretching voice");
        assert_eq!(
            filter.channels(),
            6,
            "the filter must match the reader's width, not a default"
        );
        assert!(
            plain.stretch.is_none(),
            "a non-stretching voice must not carry a filter — that is the whole \
             point of building lazily"
        );
    }

    /// A slot whose intent says stretch but whose filter has not arrived reads
    /// DRY, not silent.
    ///
    /// `active_stretch` requires both the intent and the unit. Degrading to a
    /// dry read means a late filter costs one block of un-stretched audio rather
    /// than a gap — and, critically, never an allocation in the callback.
    #[test]
    fn a_missing_stretch_filter_reads_dry_not_silent() {
        let transport = MockTransport::rolling(Beat::new(0.0), Bpm::new(120.0));
        let (mut unit, _h) = VoicePool::with_channels(Some(transport.clone()), None, 6);

        let sampler = MemorySource::with_config(
            indexed_wave(6, 512),
            MemorySourceConfig {
                channels: 6,
                placement: Some(TransportPlacement {
                    transport,
                    start_beat: Beat::new(0.0),
                    duration_beats: None,
                }),
                ..Default::default()
            },
        );
        // Insert DIRECTLY with no filter, simulating one that has not arrived.
        unit.insert_voice_with_stretch(
            SlotId(1),
            Voice {
                source: VoiceSource::Memory(sampler),
                play: Playback {
                    stretch: StretchFactor::new(2.0),
                    ..Default::default()
                },
                channel_index: None,
            },
            None,
        );

        let mut out = [0.0f32; 6];
        unit.tick(&[], &mut out);
        for (c, &s) in out.iter().enumerate() {
            assert!(
                (s - (c + 1) as f32).abs() < 1e-3,
                "channel {c} should read dry ({}), got {s} — a missing filter \
                 must not silence the slot",
                c + 1
            );
        }
    }
}
