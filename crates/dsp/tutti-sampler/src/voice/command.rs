//! The control-plane protocol: what ECS sends the audio thread, and the handle
//! it sends through.
//!
//! [`VoiceCommand`] is drained inside the audio callback, so every variant is
//! built control-side — allocation included. [`VoicePoolHandle::send`] is where
//! the stretch filter gets constructed for a voice that needs one, precisely so
//! the callback never has to.

use std::sync::Arc;

use crate::stretch;

use super::pool::Retired;
use super::slot::{stretch_values_want_filter, stretch_wanted};
use super::types::{Direction, SlotId, Voice};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use tutti_core::{
    Amplitude, Beat, BeatDuration, Cents, ChannelLayout, PlaybackRate, SamplePosition,
    SampleRate, StretchFactor, Wave,
};

/// Voice slots a reader holds before its slot vector has to grow.
///
/// The `AddVoice` drain runs in the audio callback, so `voices.push` must not
/// reallocate there. 64 covers any realistic per-track voice count; a track past
/// it pays one grow on the next add and is then stable again.
pub(super) const MAX_RESIDENT_VOICES: usize = 64;

pub(super) const COMMAND_CAPACITY: usize = 64;

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
        /// A pre-built processor for the case where the slot has none yet, same
        /// contract as [`AddVoice::stretch`](Self::AddVoice::stretch):
        /// [`VoicePoolHandle::send`] fills it in on the control thread, and the
        /// drain only moves it into the slot.
        ///
        /// This field is why turning stretch ON mid-flight works at all. A voice
        /// spawned at unity/zero gets `stretch: None` from `AddVoice` (correctly
        /// — it did not need one), so without a filter arriving here,
        /// [`VoiceSlot::set_stretch`] would flip the gate fields on a slot that
        /// has nothing to flip and the voice would read dry forever. It cannot
        /// be built in the drain: that is the audio thread, and construction
        /// allocates an FFT setup plus per-channel scratch.
        ///
        /// `None` when the slot already holds a processor (the update is then
        /// pure atomics) or when the update turns stretching off.
        ///
        /// **Unboxed, unlike [`AddVoice::stretch`](Self::AddVoice::stretch).**
        /// `VoiceSlot::stretch` is an `Option<stretch::Unit>`, so a `Box` here
        /// would have to be unboxed to install it — and moving out of a `Box`
        /// frees the box, in the drain, on the audio thread. That is a 56-byte
        /// free the no-alloc guard catches. `AddVoice` gets away with a `Box`
        /// only because `VoiceCommand` is sized to its largest variant and that
        /// one is already the largest; here the box buys nothing.
        stretch: Option<stretch::Unit>,
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
                stretch,
            } => f
                .debug_struct("UpdateStretch")
                .field("id", id)
                .field("stretch_factor", stretch_factor)
                .field("pitch_cents", pitch_cents)
                .field("stretch_prebuilt", &stretch.is_some())
                .finish(),
        }
    }
}

// ---------------------------------------------------------------------------
// Handle — held by ECS systems, sends commands to the audio-thread unit.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct VoicePoolHandle {
    pub(crate) tx: Sender<VoiceCommand>,
    /// Slots the audio thread has removed and handed back to be freed here.
    ///
    /// See [`VoicePool::retired`]. Draining this is what actually moves the
    /// deallocation off the callback; [`collect_retired`](Self::collect_retired)
    /// is the call that does it.
    pub(crate) retired: Receiver<Retired>,
    /// The reader's output width, copied at construction (it is fixed for the
    /// reader's lifetime). Lets [`send`](Self::send) build a stretch filter at
    /// the right width on the CONTROL thread — see
    /// [`VoiceCommand::AddVoice::stretch`].
    pub(crate) channels: ChannelLayout,
    /// The reader's sample rate at construction, for the same reason.
    pub(crate) sample_rate: SampleRate,
}

impl VoicePoolHandle {
    /// Free everything the audio thread has retired since the last call — removed
    /// slots and surplus stretch filters alike (see [`Retired`]).
    ///
    /// Call this periodically from the control thread — once a frame is ample.
    /// Skipping it is safe but forfeits the point: the retirement channel fills,
    /// and further retirements fall back to freeing in the audio callback.
    ///
    /// Returns how many values were freed, which is what a test can assert on.
    pub fn collect_retired(&self) -> usize {
        let mut n = 0;
        while self.retired.try_recv().is_ok() {
            n += 1;
        }
        n
    }

    /// Queue a command, doing any allocation it implies **here**, on the calling
    /// (control) thread.
    ///
    /// This is the one chokepoint every command passes through, which makes it the
    /// right place to keep the audio thread clean: `drain_commands` runs from
    /// `tick`/`process`, so anything expensive left for the drain is an allocation
    /// in the callback. Today that means materialising the stretch filter for an
    /// `AddVoice` **or an `UpdateStretch`** that needs one.
    pub fn send(&self, cmd: VoiceCommand) {
        let cmd = self.prepare(cmd);
        match self.tx.try_send(cmd) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                tracing::warn!("VoicePool command queue full, dropping command");
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
            // Same materialisation for an update that turns stretching ON. We
            // cannot check whether the slot already has a processor — that lives
            // on the audio thread — so build unconditionally when the new values
            // ask for stretch and let the drain drop a redundant one. Paying an
            // occasional wasted control-thread allocation is the right trade
            // against the alternatives: querying the slot needs a round-trip, and
            // building in the drain allocates in the callback.
            VoiceCommand::UpdateStretch {
                id,
                stretch_factor,
                pitch_cents,
                stretch: None,
            } if stretch_values_want_filter(stretch_factor, pitch_cents) => {
                let unit = stretch::Unit::with_channels(self.sample_rate, self.channels);
                unit.set_stretch_factor(stretch_factor);
                unit.set_pitch_cents(pitch_cents);
                VoiceCommand::UpdateStretch {
                    id,
                    stretch_factor,
                    pitch_cents,
                    stretch: Some(unit),
                }
            }
            other => other,
        }
    }
}
