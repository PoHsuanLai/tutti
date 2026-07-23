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

use crate::stretch;
use crate::ClipReader;
use crate::Command;
use crate::Commands;
use crate::LoopSetting;
use crate::SamplerUnit;
use crate::StreamingClipReader;
use crate::TransportPlacement;
#[cfg(feature = "bevy")]
use bevy_ecs::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use tutti_core::{
    AudioUnit, Beat, BeatDuration, BufferMut, BufferRef, Cents, Linear, Ratio, SamplePosition,
    SignalFrame, Timeline, Wave,
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
// control op (gain, placement, loop, wave swap, seek, play/stop, reset,
// set_sample_rate) is unified through the `ClipReader` trait — see
// `as_clip_reader_mut` — so the command drain no longer branches per backend.
//
// The enum (not a `Box<dyn AudioUnit>`) is deliberate: RT requires monomorphized
// dispatch on `tick`/`process`, so the per-sample read inlines and never touches
// a vtable or the heap. `ClipReader` is a trait object only on the cold path.
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
    /// The direct-read source as `&mut dyn ClipReader` — the single cold-path
    /// control surface. Collapses the former `Ram`/`Disk`-specific mutation into
    /// one `ClipReader` call, so the command drain no longer branches per
    /// backend.
    #[inline]
    fn as_clip_reader_mut(&mut self) -> &mut dyn ClipReader {
        match self {
            Self::Ram(s) => s,
            Self::Disk(s) => s,
        }
    }
}

// ---------------------------------------------------------------------------
// Playback — the control-INTENT record for one voice. It says *what* the voice
// should do (gain, speed, direction, loop, timeline placement, stretch, pitch);
// each [`VoiceSource`] APPLIES it its own way (the in-RAM `SamplerUnit` stores
// the state on its resident DSP; the streaming reader forwards to the butler's
// shared `RtState`). The apply fan-out is the cold-path `ClipReader` surface via
// [`VoiceSource::as_clip_reader_mut`] — Playback is the description, not the
// applied state.
//
// `Default` is hand-written: the newtypes default to zero, so a derived default
// would ship silent (`gain = 0`) and frozen (`speed = 0` / `stretch = 0`).
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct Playback {
    pub gain: Linear,
    pub speed: Ratio,
    pub direction: Direction,
    pub loop_: LoopSetting,
    pub placement: Option<TransportPlacement>,
    /// Time-stretch factor (1.0 = no stretch). Absorbed here so the placement,
    /// stretch, and pitch intent live in one record — `ClipSpec` no longer keeps
    /// a stretch sidecar.
    pub stretch: Ratio,
    /// Pitch shift in cents (0.0 = no shift).
    pub pitch: Cents,
}

impl Default for Playback {
    fn default() -> Self {
        Self {
            gain: Linear::new(1.0),
            speed: Ratio::new(1.0),
            direction: Direction::Forward,
            loop_: LoopSetting::Off,
            placement: None,
            stretch: Ratio::new(1.0),
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
    pub speed: Ratio,
    pub direction: Direction,
    pub looping: bool,
    pub loop_start: SamplePosition,
    pub loop_end: SamplePosition,
    pub stretch: Ratio,
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
    fn new(id: SlotId, voice: Voice, sample_rate: f64) -> Self {
        let stretch = stretch::Unit::new(sample_rate);
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
    fn set_stretch(&mut self, stretch_factor: Ratio, pitch_cents: Cents) {
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
    fn tick_frame(&mut self) -> [f32; 2] {
        let direction = self.voice.play.direction;
        let gain = self.voice.play.gain;
        if self.needs_stretch() {
            // Tick the SINGLE source once to get its raw frame (the same
            // per-variant read the else-branch uses — the `VoiceSource` enum
            // still owns the read), then feed that frame into the stretch filter.
            // Alloc-free: stack `[f32; 2]`, no heap.
            let raw = read_source_frame(&mut self.voice.source, direction, gain);
            let mut buf = [0.0f32; 2];
            self.stretch.tick(&raw, &mut buf);
            buf
        } else {
            match &mut self.voice.source {
                VoiceSource::Ram(sampler) => match sampler.transport_sample_position() {
                    Some(pos) => {
                        let (l, r) = read_clip_sample(sampler, direction, pos, gain);
                        [l, r]
                    }
                    None => [0.0, 0.0],
                },
                VoiceSource::Disk(reader) => {
                    // The `StreamingClipReader` owns its placement gate: it
                    // emits silence outside the clip window and pulls the
                    // butler ring inside it. Alloc-free (preallocated
                    // `fetch_scratch`).
                    let mut buf = [0.0f32; 2];
                    reader.tick(&[], &mut buf);
                    buf
                }
            }
        }
    }

    /// Accumulate this slot's block-rate contribution into `output`: the exact
    /// per-variant read the `process` mixdown does for a single slot. Factored so
    /// the mixer loop and the standalone [`VoiceNode`] share one definition. RT:
    /// the `VoiceSource` enum match is unchanged, only relocated here.
    #[inline]
    fn process_into(&mut self, size: usize, output: &mut BufferMut) {
        let direction = self.voice.play.direction;
        let gain = self.voice.play.gain;
        if self.needs_stretch() {
            // Per-sample: read the SINGLE source frame (same per-variant read
            // as the else-branch — the enum still owns the read), then feed
            // it through the stretch filter. Alloc-free: stack `[f32; 2]`.
            match &mut self.voice.source {
                VoiceSource::Ram(sampler) => {
                    let Some(start_pos) = sampler.transport_sample_position() else {
                        return;
                    };
                    let advance = (sampler.speed().get() * sampler.src_ratio().get()) as f64;
                    let mut tick_out = [0.0f32; 2];
                    for i in 0..size {
                        let pos = start_pos + i as f64 * advance;
                        let (l, r) = read_clip_sample(sampler, direction, pos, gain);
                        self.stretch.tick(&[l, r], &mut tick_out);
                        output.set_f32(0, i, output.at_f32(0, i) + tick_out[0]);
                        output.set_f32(1, i, output.at_f32(1, i) + tick_out[1]);
                    }
                }
                VoiceSource::Disk(reader) => {
                    let mut raw = [0.0f32; 2];
                    let mut tick_out = [0.0f32; 2];
                    for i in 0..size {
                        reader.tick(&[], &mut raw);
                        self.stretch.tick(&raw, &mut tick_out);
                        output.set_f32(0, i, output.at_f32(0, i) + tick_out[0]);
                        output.set_f32(1, i, output.at_f32(1, i) + tick_out[1]);
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
                        let (l, r) = read_clip_sample(sampler, direction, pos, gain);
                        output.set_f32(0, i, output.at_f32(0, i) + l);
                        output.set_f32(1, i, output.at_f32(1, i) + r);
                    }
                }
                VoiceSource::Disk(reader) => {
                    // Sum the reader per-sample (its placement gate + ring
                    // pull run inside each `tick`). Per-sample accumulation
                    // mirrors the stretch branch above and keeps this
                    // alloc-free — no per-slot scratch `BufferMut`.
                    let mut tick_out = [0.0f32; 2];
                    for i in 0..size {
                        reader.tick(&[], &mut tick_out);
                        output.set_f32(0, i, output.at_f32(0, i) + tick_out[0]);
                        output.set_f32(1, i, output.at_f32(1, i) + tick_out[1]);
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
fn read_clip_sample(
    sampler: &SamplerUnit,
    direction: Direction,
    pos: f64,
    gain: Linear,
) -> (f32, f32) {
    let (l, r) = match direction {
        Direction::Reverse => {
            let len = sampler.duration_samples() as f64;
            let reversed = (len - 1.0 - pos).max(0.0);
            sampler.get_sample_raw(reversed)
        }
        Direction::Forward => sampler.get_sample_raw(pos),
    };
    let g = gain.get();
    (l * g, r * g)
}

/// Read ONE stereo frame from a single voice source, using the exact per-variant
/// read the direct (non-stretch) path uses — the `VoiceSource` enum still owns
/// the read. `gain` scales the in-RAM read at the Voice level (see
/// [`read_clip_sample`]); the streaming reader applies its own gain internally.
/// Alloc-free: returns a stack frame. Used to feed the stretch filter (which
/// owns no source) on the hot path.
#[inline]
fn read_source_frame(source: &mut VoiceSource, direction: Direction, gain: Linear) -> [f32; 2] {
    match source {
        VoiceSource::Ram(sampler) => match sampler.transport_sample_position() {
            Some(pos) => {
                let (l, r) = read_clip_sample(sampler, direction, pos, gain);
                [l, r]
            }
            None => [0.0, 0.0],
        },
        VoiceSource::Disk(reader) => {
            let mut buf = [0.0f32; 2];
            reader.tick(&[], &mut buf);
            buf
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
    /// cold-path appliers the update commands use (`ClipReader::set_*` +
    /// [`Self::apply_loop`]) — no allocation or I/O on the hot path, since the
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
        speed: Ratio,
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
        stretch_factor: Ratio,
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
    pub stretch_factor: Ratio,
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
        Self {
            clips: Vec::new(),
            rx,
            sample_rate: 44100.0,
            transport,
            butler,
        }
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

    /// Number of clip slots currently materialised (drained from the command
    /// queue). Diagnostic / test helper.
    pub fn clip_count(&self) -> usize {
        self.clips.len()
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
    ///   (`ClipReader::set_loop`).
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
                sampler.set_loop(setting.clone());
                slot.voice.play.loop_ = setting;
            }
            VoiceSource::Disk(_) => {
                if let (Some(butler), Some(channel_index)) =
                    (&self.butler, slot.voice.channel_index)
                {
                    butler.send(Command::Loop {
                        channel_index,
                        setting: setting.clone(),
                    });
                }
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
    /// `Update*` commands use: `ClipReader::set_*` (unified across tiers — in-RAM
    /// stores on the `SamplerUnit`, streaming forwards to the shared `RtState`)
    /// for gain / speed / direction, and [`Self::apply_loop`] for loop (in-RAM
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
        // `ClipSlot::new` primes the resident stretch unit from `voice.play`
        // (stretch/pitch) — the one heavy step, done here off the hot path.
        self.clips.push(ClipSlot::new(id, voice, self.sample_rate));

        // Realise the remaining intent through the same appliers the update
        // commands use. The slot is now present, so `slot_mut` / `apply_loop`
        // find it by id.
        if let Some(slot) = self.slot_mut(id) {
            slot.voice.source.as_clip_reader_mut().set_gain(gain);
            slot.voice.play.gain = gain;
            slot.voice.source.as_clip_reader_mut().set_speed(speed);
            slot.voice.play.speed = speed;
            slot.voice.play.direction = direction;
            slot.voice
                .source
                .as_clip_reader_mut()
                .set_direction(direction);
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
                        // In-memory: swap the resident wave. Streaming has no
                        // in-RAM wave; its `ClipReader::set_wave` is a deliberate
                        // no-op (a source change means re-registering the butler
                        // stream on a different file — a control-thread / butler
                        // op, re-issued control-side from dawai-model as a fresh
                        // `AddVoice` with a `Disk` source). One `ClipReader` call
                        // covers both.
                        slot.voice.source.as_clip_reader_mut().set_wave(wave);
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
                            .as_clip_reader_mut()
                            .set_placement(start_beat, duration_beats);
                        if let Some(placement) = &mut slot.voice.play.placement {
                            placement.start_beat = start_beat;
                            placement.duration_beats = duration_beats;
                        }
                    }
                }
                ClipCommand::UpdateGain { id, gain } => {
                    if let Some(slot) = self.slot_mut(id) {
                        slot.voice.source.as_clip_reader_mut().set_gain(gain);
                        slot.voice.play.gain = gain;
                    }
                }
                ClipCommand::UpdateSpeed { id, speed } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // Unified across tiers: both backends carry speed in-unit.
                        // In-RAM stores it on the `SamplerUnit`; streaming forwards
                        // to the shared `RtState` (the exact speed effect of the
                        // butler's `SetVarispeed`, reachable from the reader). The
                        // `ClipReader::set_speed` covers both — dawai no longer
                        // forks streaming speed onto a separate butler command.
                        slot.voice.source.as_clip_reader_mut().set_speed(speed);
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
                        slot.voice
                            .source
                            .as_clip_reader_mut()
                            .set_direction(direction);
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
        }
    }
}

impl AudioUnit for TrackClipReaderUnit {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        for slot in &mut self.clips {
            slot.voice.source.as_clip_reader_mut().reset();
            slot.stretch.reset();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate.get();
        for slot in &mut self.clips {
            slot.voice
                .source
                .as_clip_reader_mut()
                .set_sample_rate(sample_rate);
            slot.sample_rate = sample_rate.get();
            slot.stretch.set_sample_rate(sample_rate);
        }
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        self.drain_commands();

        if output.len() < 2 {
            return;
        }

        let mut left = 0.0_f32;
        let mut right = 0.0_f32;

        // Each slot reads its ONE voice via the shared `ClipSlot::tick_frame`
        // (the same per-variant `VoiceSource` match a standalone `VoiceNode`
        // uses — factored, not duplicated, and no per-sample dyn).
        for slot in &mut self.clips {
            let frame = slot.tick_frame();
            left += frame[0];
            right += frame[1];
        }

        output[0] = left;
        output[1] = right;
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        self.drain_commands();

        for i in 0..size {
            output.set_f32(0, i, 0.0);
            output.set_f32(1, i, 0.0);
        }

        // Each slot accumulates its ONE voice via the shared
        // `ClipSlot::process_into` (the same per-variant `VoiceSource` match a
        // standalone `VoiceNode` uses).
        for slot in &mut self.clips {
            slot.process_into(size, output);
        }
    }

    audio_unit_boilerplate!(id = TRACK_CLIP_READER_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(2)
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

pub struct VoiceNode(ClipSlot);

impl VoiceNode {
    /// Wrap a single [`Voice`] as a standalone graph node. Builds the resident
    /// stretch processor once (like a mixer slot), off any hot path.
    pub fn new(voice: Voice) -> Self {
        Self(ClipSlot::new(SlotId(0), voice, 44100.0))
    }

    /// The wrapped voice (immutable view).
    pub fn voice(&self) -> &Voice {
        &self.0.voice
    }

    /// The wrapped voice (mutable view) — used by the offline render to rebind
    /// the transport via [`Voice::replace_transport`].
    pub fn voice_mut(&mut self) -> &mut Voice {
        &mut self.0.voice
    }

    /// Rebind the transport clock behind the wrapped voice's placement,
    /// preserving start / duration. Convenience delegate to
    /// [`Voice::replace_transport`] so the offline region render can rebind a
    /// standalone voice node without reaching through `voice_mut`.
    pub fn replace_transport(&mut self, transport: Arc<dyn Timeline>) {
        self.0.voice.replace_transport(transport);
    }
}

// Hand-rolled: wraps a non-`Debug` `ClipSlot`. Print the wrapped `Voice`
// (which is `Debug`) and leave the resident stretch DSP out.
impl std::fmt::Debug for VoiceNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoiceNode")
            .field("voice", &self.0.voice)
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
        Self(ClipSlot {
            id: self.0.id,
            voice: self.0.voice.clone(),
            stretch: self.0.stretch.clone(),
            sample_rate: self.0.sample_rate,
        })
    }
}

impl AudioUnit for VoiceNode {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        self.0.voice.source.as_clip_reader_mut().reset();
        self.0.stretch.reset();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.0
            .voice
            .source
            .as_clip_reader_mut()
            .set_sample_rate(sample_rate);
        self.0.sample_rate = sample_rate.get();
        self.0.stretch.set_sample_rate(sample_rate);
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        if output.len() < 2 {
            return;
        }
        // Same single-voice read the mixer runs per slot.
        let frame = self.0.tick_frame();
        output[0] = frame[0];
        output[1] = frame[1];
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, 0.0);
            output.set_f32(1, i, 0.0);
        }
        self.0.process_into(size, output);
    }

    audio_unit_boilerplate!(id = crate::node_id::VOICE_NODE_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(2)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        fn loop_range(&self) -> Option<tutti_core::LoopRange> {
            None
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

        let sampler = SamplerUnit::with_transport(
            wave.clone(),
            transport.clone(),
            Beat::new(0.0),
            None,
        );
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
            let sampler = SamplerUnit::with_transport(
                wave.clone(),
                transport.clone(),
                Beat::new(0.0),
                None,
            );
            add_ram_clip(&handle, SlotId(i), sampler);
        }

        let mut out_3 = [0.0f32; 2];
        unit.tick(&[], &mut out_3);

        let (mut unit2, handle2) = TrackClipReaderUnit::new();
        let sampler = SamplerUnit::with_transport(
            wave.clone(),
            transport.clone(),
            Beat::new(0.0),
            None,
        );
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
            stretch_factor: Ratio::new(1.0),
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
            stretch_factor: Ratio::new(1.0),
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
            stretch_factor: Ratio::new(2.0),
            pitch_cents: Cents::new(0.0),
        });
        unit.tick(&[], &mut out);
        assert!(unit.clips[0].needs_stretch(), "stretch should be active");

        handle.send(ClipCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: Ratio::new(1.0),
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
        let sampler = SamplerUnit::with_transport(
            wave.clone(),
            transport.clone(),
            Beat::new(0.0),
            None,
        );
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
            stretch_factor: Ratio::new(1.0),
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
}
