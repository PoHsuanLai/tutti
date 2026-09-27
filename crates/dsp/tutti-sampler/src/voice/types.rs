//! The voice vocabulary: what a voice *is*, before anything plays it.
//!
//! [`Voice`] is the unit the pool stores per slot — a [`VoiceSource`] (the audio
//! itself, resident or streaming) plus a [`Playback`] record of control intent.
//! Nothing here renders; [`super::slot`] does that.

use std::sync::Arc;

use super::disk_voice::DiskVoice;
use super::memory_source::{LoopSetting, MemorySource, VoiceWindow};
use super::fault::FaultLatch;
use tutti_core::{
    Amplitude, Beat, BeatDuration, Cents, PlaybackRate, SampleRate, StretchFactor,
};
use tutti_graph::{ForkCause, ForkMode, Node};

/// Opaque identifier for one voice slot in a [`VoicePool`](super::VoicePool).
///
/// Deliberately a bare `u128` and not a DAW noun: the engine assigns no meaning
/// to the value, so a host may map its own clip identity onto it without this
/// crate depending on the host's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(pub u128);

/// Playback direction for a voice.
///
/// A named pair rather than a `reverse: bool`, so the intent reads at the call
/// site instead of at the declaration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// Read the source from its start toward its end.
    #[default]
    Forward,
    /// Read the source from its end toward its start.
    Reverse,
}

impl Direction {
    /// Returns `true` for [`Reverse`](Self::Reverse).
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

/// A voice's audio source, monomorphized over the two playback tiers.
///
/// # The two variants differ only in the essential per-sample read
///
/// [`Memory`](Self::Memory) indexes an `Arc<Wave>`; [`Disk`](Self::Disk) pops
/// the butler-fed ring, emitting silence while seeking and crossfading on
/// refill. Every *cold* control op is a `match` on this enum — `apply_gain`,
/// `apply_speed`, `apply_direction` and `apply_placement`.
///
/// # There is deliberately no `ClipReader` trait
///
/// A trait over the pair is the obvious shape and it is the wrong one: half its
/// methods no-op on one side (streaming clamps speed, in-memory does not;
/// setting a wave means nothing on disk), so the trait *hides* the divergence
/// instead of removing it. With two in-crate impls the enum is the better tool —
/// it inlines, it surfaces the fork at the call site, and adding a variant makes
/// the compiler enumerate every decision to make.
///
/// The rule this generalizes: share pure *functions* across the tiers
/// (`SrcRatio::for_rates` is the pattern), never trait methods whose meaning is
/// tier-conditional.
///
/// # Real-time
///
/// The enum rather than a `Box<dyn Node>` is what the audio thread needs:
/// dispatch on the block read is monomorphized, so the per-sample read touches
/// neither a vtable nor the heap. Both variants are `Clone` and graph nodes
/// of their own, so the field-wise [`Voice`] clone and the stretch wrapper
/// work uniformly across them.
#[non_exhaustive]
pub enum VoiceSource {
    /// The whole source is resident in memory.
    Memory(MemorySource),
    /// The source streams incrementally from the butler's ring.
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
    /// Run the source at the owner's rate (what `prepare` does to the
    /// source as a node of its own: the memory tier's conversion, the disk
    /// tier's step). Allocation-free: a rate and a ratio, so the pool's
    /// `AddVoice` drain can run it.
    pub(crate) fn prepare_rate(&mut self, sample_rate: SampleRate) {
        match self {
            Self::Memory(s) => s.set_render_rate(sample_rate),
            Self::Disk(s) => s.set_render_rate(sample_rate),
        }
    }

    /// Drop the source's own mid-flight state at a jump or a reset: the
    /// memory tier's free-running cursor (a placed read has none), the disk
    /// tier's read (its crossfade and last position).
    pub(crate) fn flush(&mut self) {
        match self {
            Self::Memory(s) => s.reset(),
            Self::Disk(s) => s.flush(),
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
    /// why the bound belongs to [`PlaybackRate`] rather than to either of these
    /// arms: a bound applied in one arm is a bound the other silently skips.
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

/// The control-*intent* record for one voice: what the voice should do, not what
/// it is currently doing.
///
/// Each [`VoiceSource`] applies this its own way — the in-memory tier stores the
/// state on its resident DSP, the streaming tier forwards to the butler's shared
/// control cell — and the apply fan-out is the [`VoiceSource`] match. This is
/// the description; the source holds the applied state.
///
/// # There is deliberately no `placement` field
///
/// Placement is the source's alone. Carrying a second copy here makes two clocks
/// kept in sync by hand, and a rebind that reaches only the record leaves the
/// real read clock on the live transport — rendering silence with nothing to
/// point at. Ask the source for its placement.
#[derive(Debug)]
pub struct Playback {
    /// Output gain, applied after the source read. Linear, not decibels.
    pub gain: Amplitude,
    /// Varispeed. Couples pitch to rate; unity is `PlaybackRate::UNITY`.
    pub speed: PlaybackRate,
    /// Whether the source is read forward or reversed.
    pub direction: Direction,
    /// Loop mode and, where looping, its bounds and crossfade.
    pub loop_: LoopSetting,
    /// Time-stretch factor (`1.0` = no stretch), driving the phase vocoder.
    /// Pitch-independent, unlike [`speed`](Self::speed).
    pub stretch: StretchFactor,
    /// Pitch shift in cents (`0.0` = no shift), independent of `stretch`.
    pub pitch: Cents,
}

// Hand-written, not derived: the newtypes default to zero, so a derived default
// would ship silent (`gain = 0`) and frozen (`speed = 0` / `stretch = 0`).
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
            loop_: self.loop_,
            stretch: self.stretch,
            pitch: self.pitch,
        }
    }
}

/// One voice's playback state — what a [`VoicePool`](super::VoicePool) stores
/// per slot.
#[derive(Debug)]
pub struct Voice {
    /// The audio itself, resident or streaming.
    pub source: VoiceSource,
    /// The control intent applied to [`source`](Self::source).
    pub play: Playback,
    /// Butler channel index for a [`VoiceSource::Disk`] source; `None` for
    /// [`Memory`](VoiceSource::Memory).
    ///
    /// The pool forwards streaming loop ops to this channel over the typed
    /// [`Commands`](crate::Commands) handle. Looping on the disk tier is
    /// butler-owned — it reads a fade-in head off disk and mutates the stream
    /// plan, neither of which the reader can reach — so forwarding is the honest
    /// path rather than an indirection. Meaningless for `Memory`, where the loop
    /// is primed directly on the source.
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
    /// A copy of this voice for a fork of the node holding it, sharing
    /// nothing with it: a memory voice with its own gain cell (at the value
    /// it holds), a disk voice reading its file itself
    /// ([`DiskVoice::fork_copy`]). A disk voice forks only for an offline
    /// render: [`ForkMode::Live`] is refused for it (its fork would read its
    /// file on the audio thread).
    pub(crate) fn fork_copy(&self, mode: ForkMode<'_>) -> Result<Self, ForkCause> {
        let source = match &self.source {
            VoiceSource::Memory(s) => {
                let mut s = s.clone();
                s.detach_gain();
                VoiceSource::Memory(s)
            }
            VoiceSource::Disk(_) if matches!(mode, ForkMode::Live) => {
                return Err(ForkCause::new(super::disk_voice::LiveDiskFork));
            }
            VoiceSource::Disk(voice) => VoiceSource::Disk(voice.fork_copy()),
        };
        Ok(Self {
            source,
            play: self.play.clone(),
            channel_index: self.channel_index,
        })
    }

    /// A forked disk voice's failure latch, `None` for a memory voice (which
    /// cannot fail) or a live one.
    pub(crate) fn fault(&self) -> Option<Arc<FaultLatch>> {
        match &self.source {
            VoiceSource::Memory(_) => None,
            VoiceSource::Disk(voice) => voice.fault(),
        }
    }
}
