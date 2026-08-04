//! The voice vocabulary: what a voice *is*, before anything plays it.
//!
//! [`Voice`] is the unit the pool stores per slot — a [`VoiceSource`] (the audio
//! itself, resident or streaming) plus a [`Playback`] record of control intent.
//! Nothing here renders; [`super::slot`] does that.

use std::sync::Arc;

use super::disk_voice::DiskVoice;
use super::memory_source::{LoopSetting, MemorySource, VoiceWindow};
use tutti_core::{
    Amplitude, AudioUnit, Beat, BeatDuration, Cents, PlaybackRate, StretchFactor, Timeline,
};

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
    /// The transport clock this source reads, if any.
    ///
    /// `None` for a free-running memory source. The disk tier always has one —
    /// its gate is unconditional, so a `DiskVoice` without a clock could not
    /// decide when to play.
    pub(crate) fn timeline(&self) -> Option<Arc<dyn Timeline>> {
        match self {
            Self::Memory(s) => s.timeline(),
            Self::Disk(r) => Some(r.timeline()),
        }
    }

    pub(crate) fn as_audio_unit_mut(&mut self) -> &mut dyn AudioUnit {
        match self {
            Self::Memory(s) => s,
            Self::Disk(s) => s,
        }
    }

    /// Set the output gain. Both tiers store a linear multiplier applied after
    /// the source read, so this is genuinely one operation.
    #[inline]
    pub(crate) fn apply_gain(&mut self, gain: Amplitude) {
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
    pub(crate) fn apply_speed(&mut self, speed: PlaybackRate) {
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
    pub(crate) fn apply_direction(&mut self, direction: Direction) {
        match self {
            Self::Memory(_) => {}
            Self::Disk(s) => s.set_direction(direction),
        }
    }

    /// Update the timeline placement window.
    #[inline]
    pub(crate) fn apply_placement(&mut self, start_beat: Beat, duration: Option<BeatDuration>) {
        match self {
            Self::Memory(s) => s.set_window(VoiceWindow {
                start: start_beat,
                duration,
            }),
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
    /// Time-stretch factor (1.0 = no stretch). Absorbed here so the stretch and
    /// pitch intent live in one record rather than in a sidecar.
    ///
    /// There is deliberately **no `placement` here**. Placement lived in this
    /// record too until it was found to be write-only: it was cloned, rebound by
    /// the offline render, and asserted on in tests, but no code ever derived a
    /// position from it — every read went to the source's own copy. Two clocks
    /// kept in sync by hand, one of them never consulted, is a rebind that can
    /// silently reach the wrong one. The source owns its placement; ask it.
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
            stretch: StretchFactor::UNITY,
            pitch: Cents::new(0.0),
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
    /// Rebinds the source's own read clock — the only clock there is.
    ///
    /// It used to rebind two: this record also carried a `placement`, and the
    /// comment here warned that rebinding only *that* one would leave the real
    /// read clock on the live transport and render silence. The second clock was
    /// write-only, so it is gone; a rebind can no longer reach the wrong one.
    ///
    /// Only the `Memory` [`MemorySource`] exposes a whole-transport swap, so a
    /// `Disk` voice is unchanged here — it needs the whole offline context, not
    /// just a clock, and rebinds through [`rebind_offline`](Self::rebind_offline)
    /// instead.
    pub fn replace_transport(&mut self, transport: Arc<dyn Timeline>) {
        if let VoiceSource::Memory(sampler) = &mut self.source {
            sampler.replace_transport(transport);
        }
    }

    /// Rebind this voice onto an offline render's transport, whichever source
    /// backs it.
    ///
    /// [`replace_transport`](Self::replace_transport) covers only the `Memory`
    /// arm, and used to be the whole of the offline rebind — which meant a
    /// `Disk` voice in a pool slot silently kept the live clock and rendered
    /// against a playhead nothing advanced. A slot is not a graph vertex, so the
    /// net-wide walk never reaches it either; this is the only path that does.
    pub fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        use tutti_core::AudioUnit;
        match &mut self.source {
            VoiceSource::Memory(sampler) => sampler.rebind_offline(ctx),
            VoiceSource::Disk(voice) => voice.rebind_offline(ctx),
        }
    }

    /// Sever this voice from live-thread state, whichever source backs it.
    ///
    /// Only `Disk` holds any — the shared ring and control cell its `Clone`
    /// duplicates by `Arc`. Left unsevered, an offline render pops frames the
    /// live audio thread is waiting on.
    pub fn isolate(&mut self) {
        use tutti_core::AudioUnit;
        match &mut self.source {
            // Was a no-op while this tier shared nothing. It shares its gain
            // cell now, so a render clone would otherwise follow the live
            // voice's fader — see `MemorySource::isolate_gain`.
            VoiceSource::Memory(s) => s.isolate_gain(),
            VoiceSource::Disk(voice) => voice.isolate(),
        }
    }
}
