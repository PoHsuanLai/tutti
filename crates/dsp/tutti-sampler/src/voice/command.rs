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
    Amplitude, Beat, BeatDuration, Cents, ChannelLayout, PlaybackRate, SamplePosition, SampleRate,
    StretchFactor, Wave,
};

/// Voice slots a reader holds before its slot vector has to grow.
///
/// The `AddVoice` drain runs in the audio callback, so `voices.push` must not
/// reallocate there. 64 covers any realistic per-track voice count; a track past
/// it pays one grow on the next add and is then stable again.
pub(super) const MAX_RESIDENT_VOICES: usize = 64;

/// Commands a reader's channel holds before a send starts failing with
/// [`SendError::Full`]. The drain empties it every block, so a full queue means
/// the control thread outran a whole audio block.
pub(super) const COMMAND_CAPACITY: usize = 64;

// ---------------------------------------------------------------------------
// Commands sent from ECS → audio thread.
// ---------------------------------------------------------------------------

/// One control-plane message from the host to a reader's audio-thread unit.
///
/// Drained inside the audio callback, so every variant must be built and
/// allocated **control-side**: that is why the two stretch-carrying variants
/// have the filter handed to them rather than constructing one. Which variants
/// take effect depends on the receiver — a [`VoicePool`](super::pool::VoicePool)
/// honours all of them, a [`VoiceNode`](super::node::VoiceNode) only
/// [`UpdatePlacement`](Self::UpdatePlacement) and silently discards the rest.
///
/// `#[non_exhaustive]`: the drain is a `match` inside the callback, and a new
/// control must be addable without breaking a host's own `match`.
#[non_exhaustive]
pub enum VoiceCommand {
    /// Add a voice by handing the reader a fully-formed [`Voice`]: a
    /// [`VoiceSource`](super::types::VoiceSource) (in-memory `MemorySource` or
    /// streaming `DiskVoice`) plus its
    /// [`Playback`](super::types::Playback) control-intent record. The drain
    /// builds the slot and applies the full `Playback` per-tier by reusing the
    /// same cold-path appliers the update commands use (`VoiceSource::apply_*` +
    /// `apply_loop`) — no allocation or I/O on the hot path, since the source is
    /// built entirely on the ECS/butler side before the send.
    ///
    /// **One add command for both tiers.** The tier rides inside `source` and
    /// the butler channel inside `Voice::channel_index`, so a host does not fork
    /// on in-memory versus streaming to spawn a voice.
    AddVoice {
        /// Identifies the slot for every later command aimed at this voice. An
        /// add at an id already resident replaces that slot.
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
    /// Retire the slot with this id, filter and all.
    ///
    /// The drain does not drop the slot in the callback — it hands it to the
    /// control thread through the retirement channel, because freeing a
    /// [`stretch::Unit`] frees its vocoder bank. See
    /// [`VoicePoolHandle::collect_retired`].
    Remove(SlotId),
    /// Point an **in-memory** slot at a different wave, keeping its placement,
    /// gain and loop.
    ///
    /// Ignored (with a warning) for a streaming slot: swapping a disk source
    /// means re-registering the butler stream on a different file, which is a
    /// control-thread operation issued as a fresh [`AddVoice`](Self::AddVoice).
    ReplaceWave {
        /// The slot to re-point.
        id: SlotId,
        /// The new wave. Shared rather than copied — several voices may read one.
        wave: Arc<Wave>,
    },
    /// Move the voice's timeline window. The one control that cannot ride
    /// `AudioUnit::set`, whose `Setting` is a single `f32`; see
    /// [`VoiceNodeHandle::set_placement`].
    UpdatePlacement {
        /// The slot to move. `SlotId(0)` for a [`VoiceNode`](super::node::VoiceNode),
        /// whose drain ignores the field.
        id: SlotId,
        /// Where the voice starts on the timeline, in [`Beat`]s.
        start_beat: Beat,
        /// How long it plays, in [`BeatDuration`]. `None` plays to the end of
        /// the source.
        duration_beats: Option<BeatDuration>,
    },
    /// Set the voice's playback gain.
    UpdateGain {
        /// The slot to set.
        id: SlotId,
        /// Linear [`Amplitude`], not [`Db`](tutti_core::Db) — the conversion is
        /// the host's.
        gain: Amplitude,
    },
    /// Set the voice's varispeed. Both tiers carry speed in-unit: in-memory
    /// stores it on the `MemorySource`, streaming forwards it to the shared
    /// state the butler ring also reads, so one command reaches both.
    UpdateSpeed {
        /// The slot to set.
        id: SlotId,
        /// [`PlaybackRate`] relative to the source's own rate; `1.0` is unity.
        /// Pitch moves with it — this is varispeed, not time-stretch.
        speed: PlaybackRate,
    },
    /// Set or clear the voice's loop region.
    ///
    /// Routed by tier: in-memory primes the range on the `MemorySource`,
    /// streaming forwards to the butler, which owns the loop-start fadein head
    /// the reader cannot reach. A streaming voice with no butler channel drops
    /// the command **and does not record the intent** — a `Playback` claiming a
    /// loop the stream never set would be replayed as real on the next insert.
    UpdateLoop {
        /// The slot to set.
        id: SlotId,
        /// `false` clears the loop, ignoring the three fields below.
        looping: bool,
        /// Loop start, in FRAMES from the head of the source
        /// ([`SamplePosition`], f64 so a long file keeps sub-frame precision).
        loop_start: SamplePosition,
        /// Loop end, in FRAMES from the head of the source.
        loop_end: SamplePosition,
        /// Crossfade length in FRAMES (not samples) — the same denomination as
        /// the two positions above. `0` butt-splices.
        crossfade_frames: usize,
    },
    /// Clear the voice's loop. Equivalent to [`UpdateLoop`](Self::UpdateLoop)
    /// with `looping: false`, and spelled separately so a host clearing a loop
    /// need not invent positions it is about to discard.
    ClearLoop(SlotId),
    /// Play the voice forwards or backwards.
    ///
    /// One command reaches both tiers, which is why it is not folded into
    /// [`UpdateSpeed`](Self::UpdateSpeed): in-memory drives a reversed index in
    /// the hot read, streaming forwards to the shared state the butler ring
    /// reads.
    UpdateReverse {
        /// The slot to set.
        id: SlotId,
        /// [`Direction::Forward`] or [`Direction::Reverse`].
        direction: Direction,
    },
    /// Set the voice's time-stretch factor and pitch shift — the two controls
    /// that change duration and pitch **independently**, unlike
    /// [`UpdateSpeed`](Self::UpdateSpeed).
    ///
    /// Pool-only in practice: a [`VoiceNode`](super::node::VoiceNode)'s drain
    /// discards it, so a node's pitch is fixed at construction and a host that
    /// wants to change it respawns the voice.
    UpdateStretch {
        /// The slot to set.
        id: SlotId,
        /// Duration multiplier ([`StretchFactor`]); `1.0` is unity. Pitch is
        /// unchanged.
        stretch_factor: StretchFactor,
        /// Pitch shift in [`Cents`]; `0.0` is unity. Duration is unchanged.
        pitch_cents: Cents,
        /// A pre-built processor for the case where the slot has none yet, same
        /// contract as [`AddVoice::stretch`](Self::AddVoice::stretch):
        /// [`VoicePoolHandle::send`] fills it in on the control thread, and the
        /// drain only moves it into the slot.
        ///
        /// This field is why turning stretch ON mid-flight works at all. A voice
        /// spawned at unity/zero gets `stretch: None` from `AddVoice` (correctly
        /// — it did not need one), so without a filter arriving here,
        /// `VoiceSlot::set_stretch` would flip the gate fields on a slot that
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
                crossfade_frames,
            } => f
                .debug_struct("UpdateLoop")
                .field("id", id)
                .field("looping", looping)
                .field("loop_start", loop_start)
                .field("loop_end", loop_end)
                .field("crossfade_frames", crossfade_frames)
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

/// A [`VoiceCommand`] never reached the pool, and the work it carried is gone.
///
/// Modelled on [`QueueFull`](tutti_core::transport::QueueFull) one crate down:
/// the two failures are told apart because a caller acts on them differently,
/// and the lost command rides along so it can be retried or logged with its
/// contents rather than as an anonymous count.
///
/// A dropped `AddVoice` is a note that never sounds; a dropped `Remove` is a
/// voice that never stops.
#[derive(Debug)]
pub enum SendError {
    /// The 64-slot command queue was full. Transient: the audio thread drains
    /// it every block, so a caller can back off and retry.
    Full(VoiceCommand),
    /// The [`VoicePool`](super::pool::VoicePool) has been dropped, so nothing
    /// will ever drain the queue again.
    ///
    /// **Permanent** — every subsequent send fails the same way, so a caller
    /// that sees this should stop rather than retry. Reporting it is what makes
    /// a dead pool distinguishable from a working one.
    Disconnected(VoiceCommand),
}

impl SendError {
    /// The command that was lost, for a caller that wants to retry or log it.
    pub fn into_command(self) -> VoiceCommand {
        match self {
            Self::Full(cmd) | Self::Disconnected(cmd) => cmd,
        }
    }

    /// Whether this failure is permanent — no later send can succeed either.
    ///
    /// The distinction worth acting on: `Full` means "try again", `Disconnected`
    /// means "stop trying".
    pub fn is_disconnected(&self) -> bool {
        matches!(self, Self::Disconnected(_))
    }
}

impl core::fmt::Display for SendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Full(_) => write!(f, "voice command queue full; the command was dropped"),
            Self::Disconnected(_) => {
                write!(f, "voice pool is gone; the command was dropped")
            }
        }
    }
}

impl std::error::Error for SendError {}

/// The control-thread end of a standalone [`VoiceNode`](super::node::VoiceNode)'s
/// command channel.
///
/// # Why a separate handle rather than reusing [`VoicePoolHandle`]
///
/// A pool addresses its voices by [`SlotId`] and hands retired slots back for
/// the control thread to free. A `VoiceNode` holds exactly one voice for its
/// whole life: there is no id to address and nothing is ever retired, so both of
/// those fields would be dead weight that every call site has to supply a
/// meaningless value for. The commands it accepts are also a strict subset —
/// `AddVoice` / `Remove` have no meaning for a node that *is* its voice.
///
/// So this carries the sender and nothing else, and its methods name the voice
/// implicitly. The wire format is shared: [`VoiceCommand`] is the same enum, and
/// the node's drain is the same `try_recv` loop, so a command gains a consumer
/// here without gaining a second definition.
#[derive(Clone, Debug)]
pub struct VoiceNodeHandle {
    /// Sends into the node's bounded command queue. No retirement receiver
    /// beside it: a node's single voice lives as long as the node does, so
    /// nothing is ever handed back to be freed.
    pub(crate) tx: Sender<VoiceCommand>,
}

impl VoiceNodeHandle {
    /// Move the voice's timeline window.
    ///
    /// **The one control that cannot ride `AudioUnit::set`**, which is the whole
    /// reason this channel exists. `Setting` carries a single `f32`, and a
    /// placement is a [`Beat`] (`f64`) plus an optional [`BeatDuration`] — two
    /// values, and a precision the transport cannot afford to lose. Truncating a
    /// beat position to `f32` re-introduces the ~2²⁴ cliff that
    /// `Sample.loop_start` was moved to `SamplePosition` (f64) to escape.
    ///
    /// Flattened into scalar fields rather than sent as a `VoiceWindow`, which is
    /// the crate's existing answer for a multi-field control — see
    /// [`VoiceCommand::UpdatePlacement`], which a pool has drained since before
    /// this handle existed.
    ///
    /// The `SlotId` is `SlotId(0)`: a node's single voice is built with that id
    /// (`VoiceNode::with_channels`), and the drain ignores it. It is in the wire
    /// format because the format is shared with the pool, not because a node has
    /// slots.
    pub fn set_placement(
        &self,
        start_beat: Beat,
        duration_beats: Option<BeatDuration>,
    ) -> Result<(), SendError> {
        self.send(VoiceCommand::UpdatePlacement {
            id: SlotId(0),
            start_beat,
            duration_beats,
        })
    }

    /// Queue a command for the node's next block.
    ///
    /// Public so a host can send anything the node's drain understands, and
    /// fallible for the reason [`VoicePoolHandle::send`] gives: a full queue is
    /// reported rather than logged and forgotten, so a caller can back off
    /// instead of silently dropping a user's edit.
    ///
    /// **A node's drain understands exactly one variant: [`VoiceCommand::UpdatePlacement`].**
    /// `VoiceNode::drain_commands` is an `if let` with no other arm, so every
    /// other variant — `UpdateSpeed`, `UpdateStretch`, `UpdateGain`, `AddVoice` —
    /// is accepted here, queued, drained and **dropped without a word**. `Ok(())`
    /// means "queued", not "will take effect".
    ///
    /// That silence is deliberate (the wire format is shared with the pool, and
    /// panicking in an audio callback is worse), but it makes this the wrong door
    /// for anything but placement. A single-voice node's gain rides
    /// `AudioUnit::set`; its speed and pitch are fixed at construction, so a host
    /// changing either respawns the voice. Only [`VoicePool`](crate::VoicePool)
    /// has the live-update path.
    pub fn send(&self, command: VoiceCommand) -> Result<(), SendError> {
        // The variants carry the lost command, deliberately — see `SendError`.
        // Dropping it here would leave a caller able to see *that* an edit
        // failed but not *which*, which is the difference between backing off
        // and giving up.
        self.tx.try_send(command).map_err(|e| match e {
            TrySendError::Full(cmd) => SendError::Full(cmd),
            TrySendError::Disconnected(cmd) => SendError::Disconnected(cmd),
        })
    }
}

/// The control-thread end of a [`VoicePool`](super::pool::VoicePool)'s command
/// channel, and the collection point for what the audio thread retires.
///
/// Held by the host (an ECS system, typically) and cloned freely — the sender is
/// multi-producer. It mirrors the reader's width and sample rate so
/// [`send`](Self::send) can build a stretch filter that matches, on **this**
/// thread; both are fixed for the reader's lifetime, so the copies cannot go
/// stale.
#[derive(Clone, Debug)]
pub struct VoicePoolHandle {
    /// Sends into the reader's bounded command queue.
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
    /// slots and surplus stretch filters alike.
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
    /// in the callback. That means materialising the stretch filter for an
    /// `AddVoice` **or an `UpdateStretch`** that needs one.
    ///
    /// `Ok` means only that the command was *queued* — the pool applies it on the
    /// audio thread, and that outcome is not available synchronously.
    ///
    /// # Errors
    ///
    /// `Err` means the command is **gone**: a dropped `AddVoice` is a note that
    /// never sounds, a dropped `Remove` a voice that never stops. That is a
    /// failure a caller can act on rather than log, which is why it is
    /// `#[must_use]` — see [`SendError::is_disconnected`] for which of the two
    /// is worth retrying.
    #[must_use = "a dropped command is a note that never sounds or a voice that never stops"]
    pub fn send(&self, cmd: VoiceCommand) -> Result<(), SendError> {
        let cmd = self.prepare(cmd);
        self.tx.try_send(cmd).map_err(|e| match e {
            TrySendError::Full(cmd) => SendError::Full(cmd),
            TrySendError::Disconnected(cmd) => SendError::Disconnected(cmd),
        })
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
            // Same materialisation for an update that turns stretching ON.
            // Whether the slot already has a processor is audio-thread state and
            // unreadable from here, so build unconditionally when the new values
            // ask for stretch and let the drain retire a redundant one. Paying an
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::pool::VoicePool;

    /// A full queue is reported to the caller, not logged and forgotten.
    ///
    /// A `tracing::warn!` reaches an operator reading logs, never the code that
    /// could back off or retry — and a dropped `AddVoice` is a note that never
    /// sounds.
    #[test]
    fn a_full_queue_is_reported_to_the_caller() {
        // Build the pool but never drain it, so the queue fills.
        let (_pool, handle) = VoicePool::new();

        for _ in 0..COMMAND_CAPACITY {
            handle
                .send(VoiceCommand::Remove(SlotId(1)))
                .expect("within capacity");
        }

        let err = handle
            .send(VoiceCommand::Remove(SlotId(2)))
            .expect_err("past capacity the command is dropped");
        assert!(
            !err.is_disconnected(),
            "a full queue is transient, not a dead pool"
        );
        assert!(matches!(err, SendError::Full(_)));
    }

    /// A dropped pool is reported as permanent. Without it every later send is a
    /// no-op with nothing to notice it by.
    #[test]
    fn a_dropped_pool_is_reported_as_disconnected() {
        let (pool, handle) = VoicePool::new();
        drop(pool);

        let err = handle
            .send(VoiceCommand::Remove(SlotId(1)))
            .expect_err("nothing will ever drain this queue again");
        assert!(
            err.is_disconnected(),
            "a dead pool must be distinguishable from a merely full one — the \
             caller should stop, not retry"
        );
    }

    /// The lost command comes back, so a caller can retry or log its contents
    /// rather than an anonymous failure.
    #[test]
    fn the_dropped_command_is_returned() {
        let (pool, handle) = VoicePool::new();
        drop(pool);

        let err = handle.send(VoiceCommand::Remove(SlotId(7))).unwrap_err();
        match err.into_command() {
            VoiceCommand::Remove(id) => assert_eq!(id, SlotId(7)),
            _ => panic!("the returned command must be the one that was lost"),
        }
    }
}
