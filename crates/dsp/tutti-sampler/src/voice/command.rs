//! The control-plane protocol: what ECS sends the audio thread, and the handle
//! it sends through.
//!
//! [`VoiceCommand`] is drained inside the audio callback, so every variant is
//! built control-side — allocation included. [`VoicePoolHandle::send`] is where
//! the stretch filter gets constructed for a voice that needs one, precisely so
//! the callback never has to.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use crate::ports::Commands;
use crate::stretch;

use super::memory_source::{LoopSetting, VoiceWindow};
use super::pool::{forward_loop, DrainCounters, LoopRoute, PoolDrops, Retired, StreamAddr};
use super::slot::{stretch_values_want_filter, stretch_wanted};
use super::types::{Direction, SlotId, Voice};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use tutti_core::{
    Amplitude, ArcLend, Beat, BeatDuration, Cents, ChannelLayout, Param, PlaybackRate,
    SamplePosition, SampleRate, StretchFactor,
};
use tutti_graph::ParamSet;
use tutti_io::Wave;

/// A node's sample rate, shared with its handle: the node stores it when it
/// is prepared (control thread), the handle reads it to build a stretch
/// filter at that rate (control thread). A scalar: no audio-thread read.
#[derive(Clone, Debug)]
pub(crate) struct SharedRate(Arc<AtomicU64>);

impl SharedRate {
    pub(crate) fn new(rate: SampleRate) -> Self {
        Self(Arc::new(AtomicU64::new(rate.get().to_bits())))
    }

    pub(crate) fn store(&self, rate: SampleRate) {
        self.0.store(rate.get().to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn load(&self) -> SampleRate {
        SampleRate(f64::from_bits(self.0.load(Ordering::Relaxed)))
    }
}

/// Voice slots a pool reserves, and the most its `AddVoice` drain will hold.
///
/// The drain runs in the audio callback, so `voices.push` must not reallocate
/// there: an add for a new slot once the pool is full is refused, handed back
/// through the retirement channel and counted
/// ([`PoolDrops::voices_refused`]).
/// 64 covers any realistic per-track voice count.
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
/// have the filter handed to them rather than constructing one. A
/// [`VoicePool`](super::pool::VoicePool) is the one receiver, and honours all
/// of them; a [`VoiceNode`](super::node::VoiceNode) takes none (its
/// [`VoiceNodeHandle::set_placement`] is its `UpdatePlacement`).
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
        /// per-sample loop), so the indirection costs nothing audible. The box
        /// itself moves into the pool's slot — the drain never unboxes it, so
        /// it frees nothing — and leaves through the retirement channel with
        /// the slot.
        voice: Box<Voice>,
        /// A stretch filter built by the SENDER, on the control thread, when
        /// `voice.play` asks for stretching.
        ///
        /// The drain runs inside `process`, so building this there would
        /// allocate in the audio callback (an FFT setup plus two block-scratch
        /// buffers per channel). [`VoicePoolHandle::send`] fills it in
        /// before the command is queued; the drain only moves it into the slot.
        ///
        /// `None` when the voice does not stretch, which is the common case and
        /// costs nothing.
        ///
        /// Unboxed, as `UpdateStretch`'s is: moving a `Unit` out of a `Box`
        /// frees the box, a deallocation on the audio thread. A `Unit` moves as
        /// a pointer anyway (its state is boxed inside it).
        stretch: Option<stretch::Unit>,
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
    /// Ignored, and counted
    /// ([`PoolDrops::wave_swaps_ignored`]),
    /// for a streaming slot: swapping a disk source
    /// means re-registering the butler stream on a different file, which is a
    /// control-thread operation issued as a fresh [`AddVoice`](Self::AddVoice).
    ///
    /// The wave the drain lets go of (the slot's old one, or this one when no
    /// in-memory slot takes it) is never dropped in the callback: it leaves
    /// through the retirement channel, since it may be the last reference to
    /// a decoded buffer. See [`VoicePoolHandle::collect_retired`].
    ReplaceWave {
        /// The slot to re-point.
        id: SlotId,
        /// The new wave. Shared rather than copied — several voices may read one.
        wave: Arc<Wave>,
    },
    /// Move the voice's timeline window: a [`Beat`] (`f64`) and an optional
    /// duration, so not a scalar param; see [`VoiceNodeHandle::set_placement`].
    UpdatePlacement {
        /// The slot to move.
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
    /// Routed by tier: in-memory primes the range on the `MemorySource` in
    /// the drain; streaming is forwarded to the butler, which owns the
    /// loop-start fadein head the reader cannot reach, by
    /// [`VoicePoolHandle::send`] on the calling thread, before the command is
    /// queued. A streaming voice whose forward fails (no butler channel, a
    /// butler that exited) is not queued: the loss is counted
    /// ([`PoolDrops`]), **and the intent is not recorded** — a `Playback`
    /// claiming a loop the stream never set would be replayed as real on the
    /// next insert. The drain records a streaming slot's loop only when the
    /// handle forwarded it to that slot's own stream
    /// ([`PoolDrops::loops_not_forwarded`] otherwise).
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
    /// Pool-only: a [`VoiceNode`](super::node::VoiceNode) has no command to
    /// take it, so a node's pitch is fixed at construction and a host that
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
        /// `PlaybackSlot::set_stretch` would flip the gate fields on a slot that
        /// has nothing to flip and the voice would read dry forever. It cannot
        /// be built in the drain: that is the audio thread, and construction
        /// allocates an FFT setup plus per-channel scratch.
        ///
        /// `None` when the slot already holds a processor (the update is then
        /// pure atomics) or when the update turns stretching off.
        ///
        /// **Unboxed, unlike [`AddVoice::stretch`](Self::AddVoice::stretch).**
        /// `PlaybackSlot::stretch` is an `Option<stretch::Unit>`, so a `Box` here
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

/// A [`VoiceCommand`] as the handle queues it for the drain.
///
/// `forwarded` is the butler stream the handle sent a loop command's
/// setting to before queueing it (`None` for every other command, and for a
/// loop it did not forward). The drain records a streaming slot's loop only
/// when this names that slot's stream, so whether a loop reached the butler
/// is decided from the command itself, never from what the handle believed
/// the slot held. Only [`VoicePoolHandle::send`] builds one.
pub(crate) struct Queued {
    pub(crate) cmd: VoiceCommand,
    pub(crate) forwarded: Option<StreamAddr>,
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

/// The controls of a standalone [`VoiceNode`](super::node::VoiceNode): what
/// its `IntoNode` hands back. Its gain, a scalar cell the node reads once per
/// block, and its placement, a published window the node reads once per
/// block.
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
/// So this carries the placement cell and the gain, and its methods name the
/// voice implicitly. There is no command queue at all: a placement is the
/// node's one non-scalar control and only the latest one matters, so it is a
/// value the handle publishes rather than a message the node must receive.
/// Nothing a caller does here can hand the node a payload (a boxed `Voice`, a
/// wave, a stretch filter) to drop on the audio thread.
///
/// ```compile_fail
/// # use tutti_sampler::{MemorySource, Playback, Voice, VoiceCommand, VoiceNode, VoiceSource};
/// # use tutti_io::Wave;
/// # use std::sync::Arc;
/// # let wave = Arc::new(Wave::from_samples(48_000.0, &[0.0; 64]));
/// # let voice = Voice {
/// #     source: VoiceSource::Memory(MemorySource::new(wave.clone())),
/// #     play: Playback::default(),
/// #     channel_index: None,
/// # };
/// let (_node, handle) = VoiceNode::new(voice).with_handle();
/// // A standalone voice node has no door for a pool command.
/// let _ = handle.send(VoiceCommand::ReplaceWave { id: tutti_sampler::SlotId(0), wave });
/// ```
///
/// The same setup with the one control it does take compiles:
///
/// ```
/// # use tutti_sampler::{MemorySource, Playback, Voice, VoiceCommand, VoiceNode, VoiceSource};
/// # use tutti_io::Wave;
/// # use std::sync::Arc;
/// # let wave = Arc::new(Wave::from_samples(48_000.0, &[0.0; 64]));
/// # let voice = Voice {
/// #     source: VoiceSource::Memory(MemorySource::new(wave.clone())),
/// #     play: Playback::default(),
/// #     channel_index: None,
/// # };
/// let (_node, handle) = VoiceNode::new(voice).with_handle();
/// handle.set_placement(tutti_core::Beat::new(1.0), None);
/// ```
#[derive(Clone, Debug)]
pub struct VoiceNodeHandle {
    /// The node's placement, shared with the node (which reads it once per
    /// block) and with its fork source (which reads it at fork time).
    pub(crate) placement: PlacementCell,
    /// The window last published to `placement`, for the control side (the
    /// fork source reads it): never a read of the node's cell.
    pub(crate) placed: PlacedWindow,
    /// The voice's gain: the cell the node reads once per block.
    pub(crate) gain: Param<Amplitude>,
    /// The gain by address ([`UnitParam::Volume`](tutti_core::UnitParam)),
    /// for a host that sets params by address.
    pub(crate) params: ParamSet,
}

/// A [`VoiceNode`](super::node::VoiceNode)'s timeline window, latest wins:
/// `None` until the handle first sets one, and the node keeps the window it
/// was built with until then.
///
/// The handle publishes (control thread), and records the same window in its
/// [`PlacedWindow`]; the node reads the cell once per block, and applies it
/// when it differs from the window it last applied; the fork source reads the
/// record when a fork is taken, so a clip moved after the insert exports where
/// it was moved to.
///
/// Latest-wins loses nothing a queue would have delivered: applying a
/// window is a plain overwrite of the source's window (no seek, no re-arm),
/// so applying every intermediate window in one block ends in the same
/// state as applying the last, and nothing else rides with it to be ordered
/// against.
pub(crate) type PlacementCell = Arc<ArcLend<Option<VoiceWindow>>>;

/// The window last published to a [`PlacementCell`], for the control side:
/// what a fork starts from. Written under its lock in the same call that
/// publishes, so the two never disagree once it returns. The node never
/// takes this lock.
pub(crate) type PlacedWindow = Arc<Mutex<Option<VoiceWindow>>>;

impl VoiceNodeHandle {
    /// Moves the voice's timeline window, from the node's next block on.
    ///
    /// A placement is a [`Beat`] (`f64`) plus an optional [`BeatDuration`] —
    /// two values, and a precision the transport cannot afford to lose, so it
    /// is no scalar param (an `f32` beat position loses whole frames past
    /// ~2²⁴). It is published to a cell the node reads once per block:
    /// never lost and never queued, and only the latest window is heard (a
    /// window set twice between two blocks plays the second). A fork of the
    /// node taken after this call starts from it.
    ///
    /// **Control thread**: publishing allocates the new value and frees the
    /// one it replaces, here, so the node never does either.
    pub fn set_placement(&self, start_beat: Beat, duration_beats: Option<BeatDuration>) {
        let window = Some(VoiceWindow {
            start: start_beat,
            duration: duration_beats,
        });
        // Held across the publish, so two handles racing leave the record
        // and the cell on the same window.
        let mut placed = self.placed.lock().unwrap_or_else(PoisonError::into_inner);
        *placed = window;
        self.placement.publish(Arc::new(window));
        drop(placed);
    }

    /// Frees the windows the node has let go of since the last
    /// [`set_placement`](Self::set_placement) (one a block still held when it
    /// was replaced), without publishing; returns how many are still held.
    /// Control thread, from the owner's tick (`bevy-tutti` sweeps every
    /// voice entity's handle each frame); with nothing retired it is a lock
    /// and a length check.
    pub fn reclaim(&self) -> usize {
        self.placement.reclaim()
    }

    /// Sets the voice's gain, from the node's next block on. A write to a
    /// shared cell: never lost, never queued.
    pub fn set_gain(&self, gain: Amplitude) {
        self.params.set(tutti_core::UnitParam::Volume, gain.get());
    }

    /// The voice's gain, as last set.
    pub fn gain(&self) -> Amplitude {
        self.gain.load()
    }

    /// The node's params by address: its gain, as
    /// [`UnitParam::Volume`](tutti_core::UnitParam::Volume).
    pub fn params(&self) -> &ParamSet {
        &self.params
    }
}

/// The control-thread end of a [`VoicePool`](super::pool::VoicePool)'s command
/// channel, and the collection point for what the audio thread retires.
///
/// Held by the host (an ECS system, typically) and cloned freely — the sender is
/// multi-producer. It mirrors the reader's width, and shares the rate the
/// reader was last prepared at, so [`send`](Self::send) can build a stretch
/// filter that matches, on **this** thread. It holds the pool's butler (the
/// pool gives it up in [`with_handle`](super::pool::VoicePool::with_handle)),
/// and knows which butler stream each streaming voice it added reads, so
/// `send` forwards a streaming loop to the butler itself: the pool's drain
/// has nothing to send to the butler with.
///
/// # Ids the drain refused
///
/// An add the drain refuses at the voice cap
/// ([`PoolDrops::voices_refused`]) still leaves its id in the handle's stream
/// table, until a [`Remove`](VoiceCommand::Remove) or another add at that id:
/// a host that gives every note a fresh id should `Remove` the ids it sees
/// refused (they come back through
/// [`collect_retired`](Self::collect_retired)), or the table grows by one
/// entry per refused streaming add. A loop sent for such an id reaches a
/// stream nothing plays, and the drain finds no slot to record it on.
#[derive(Clone, Debug)]
pub struct VoicePoolHandle {
    /// Sends into the reader's bounded command queue.
    pub(crate) tx: Sender<Queued>,
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
    /// The reader's sample rate, as its `prepare` last stored it, for the
    /// same reason.
    pub(crate) rate: SharedRate,
    /// What the pool's drain dropped or refused; read by
    /// [`drops`](Self::drops).
    pub(crate) counters: Arc<DrainCounters>,
    /// The pool's butler, which a streaming voice's loop is forwarded to.
    /// `None` for a pool built without one.
    pub(crate) butler: Option<Commands>,
    /// The butler stream of every streaming voice this handle (or a clone)
    /// added, by slot, with the loop last forwarded to it; a slot holding a
    /// memory voice has no entry. Written by `send` as it queues an add, a
    /// remove or a streaming loop, and seeded from the voices a pool held
    /// when its handle was made.
    ///
    /// It decides only where `send` forwards a loop, never whether the
    /// drain records one: a voice inserted with
    /// [`insert_voice`](super::pool::VoicePool::insert_voice) after the
    /// handle was made is not in it, and the drain then refuses that slot's
    /// loops ([`PoolDrops::loops_not_forwarded`]).
    pub(crate) streams: StreamTable,
    /// Held across every butler forward, so forwards (and the commands
    /// queued behind them) reach the butler and the pool in one order. The
    /// stream table's lock is never held across a forward, so a command
    /// that sends nothing to the butler never waits on one.
    pub(crate) forwarding: Arc<Mutex<()>>,
}

/// [`VoicePoolHandle::streams`]. Locked only for a lookup or for queueing
/// (so the table and the queue's order agree), never across a butler
/// forward. Control threads only.
pub(crate) type StreamTable = Arc<Mutex<HashMap<SlotId, StreamEntry>>>;

/// A streaming slot's entry in a [`StreamTable`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct StreamEntry {
    /// Its butler stream; `None` for a streaming voice with no butler
    /// channel.
    pub(crate) addr: Option<StreamAddr>,
    /// The loop last forwarded to `addr` with a command the pool then
    /// queued: what a forward whose command cannot be queued rolls the
    /// butler back to.
    pub(crate) loop_: LoopSetting,
}

impl VoicePoolHandle {
    /// Frees everything the audio thread has retired since the last call — removed
    /// slots, surplus stretch filters and replaced waves alike.
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

    /// What the pool's drain has dropped, refused, or freed in the callback
    /// since the pool was built. Poll it as often as convenient: the counts
    /// only grow.
    pub fn drops(&self) -> PoolDrops {
        self.counters.snapshot()
    }

    /// Queues a command, doing any allocation it implies **here**, on the calling
    /// (control) thread.
    ///
    /// This is the one chokepoint every command passes through, which makes it the
    /// right place to keep the audio thread clean: `drain_commands` runs from
    /// `process`, so anything expensive left for the drain is an allocation
    /// in the callback. That means materialising the stretch filter for an
    /// `AddVoice` **or an `UpdateStretch`** that needs one.
    ///
    /// `Ok` means only that the command was *queued* — the pool applies it on the
    /// audio thread, and that outcome is not available synchronously.
    ///
    /// # A streaming voice's loop
    ///
    /// An [`UpdateLoop`](VoiceCommand::UpdateLoop) or
    /// [`ClearLoop`](VoiceCommand::ClearLoop) aimed at a streaming voice, and
    /// an [`AddVoice`](VoiceCommand::AddVoice) of one, forward the loop to
    /// the pool's butler (`Command::Loop`, naming the voice's stream) here,
    /// and queue the command only once the butler has taken it, so the
    /// voice's `Playback` never records a loop the butler did not receive.
    /// A forward that cannot be made (a pool without a butler, a voice
    /// without a butler channel, a butler that exited) is counted
    /// ([`PoolDrops::loops_no_butler`], [`PoolDrops::loops_butler_gone`]);
    /// the loop command is then not queued (it returns `Ok`), and an add is
    /// queued with its loop `Off`.
    ///
    /// A loop sent while another thread removes its slot, or replaces it
    /// with an in-memory voice, is ordered before that command: it reaches
    /// the old stream and is not queued.
    ///
    /// # Blocking
    ///
    /// That forward waits while the butler's queue is full, as
    /// [`Commands::send`] does: as long as a stalled butler stalls, and for
    /// good on a butler nobody drains (a controller never started, or a
    /// `DiskStreamer::manual` stepped on this thread). While it waits,
    /// another thread's streaming add or streaming loop through a clone of
    /// this handle waits with it, so the butler and the pool see forwards in
    /// one order. Every other command (a remove, an in-memory add or loop,
    /// every update but a loop) never waits on the butler.
    ///
    /// # Errors
    ///
    /// `Err` means the command is **gone**: a dropped `AddVoice` is a note that
    /// never sounds, a dropped `Remove` a voice that never stops. For a
    /// streaming loop or add, the butler is left on the loop it had: a full
    /// queue is refused before the forward, and a forward whose command then
    /// cannot be queued (another thread filled the queue meanwhile) is
    /// undone by forwarding the stream's previous loop again (`Off` for a
    /// stream the handle had not added). That is a failure a caller can act
    /// on rather than log, which is why it is `#[must_use]` — see
    /// [`SendError::is_disconnected`] for which of the two is worth retrying.
    #[must_use = "a dropped command is a note that never sounds or a voice that never stops"]
    pub fn send(&self, cmd: VoiceCommand) -> Result<(), SendError> {
        match self.prepare(cmd) {
            VoiceCommand::AddVoice { id, voice, stretch } => match voice.loop_route() {
                LoopRoute::Memory => {
                    let mut streams = self.streams();
                    self.queue(VoiceCommand::AddVoice { id, voice, stretch }, None)?;
                    streams.remove(&id);
                    Ok(())
                }
                LoopRoute::Stream(addr) => self.send_stream_add(id, voice, stretch, addr),
            },
            VoiceCommand::Remove(id) => {
                let mut streams = self.streams();
                self.queue(VoiceCommand::Remove(id), None)?;
                streams.remove(&id);
                Ok(())
            }
            VoiceCommand::UpdateLoop {
                id,
                looping,
                loop_start,
                loop_end,
                crossfade_frames,
            } => self.send_loop(
                id,
                loop_setting(looping, loop_start, loop_end, crossfade_frames),
                VoiceCommand::UpdateLoop {
                    id,
                    looping,
                    loop_start,
                    loop_end,
                    crossfade_frames,
                },
            ),
            VoiceCommand::ClearLoop(id) => {
                self.send_loop(id, LoopSetting::Off, VoiceCommand::ClearLoop(id))
            }
            cmd => self.queue(cmd, None),
        }
    }

    /// Queues the add of a streaming voice on `addr`, once the butler has
    /// taken its loop (queued `Off` when the forward cannot be made).
    fn send_stream_add(
        &self,
        id: SlotId,
        mut voice: Box<Voice>,
        stretch: Option<stretch::Unit>,
        addr: Option<StreamAddr>,
    ) -> Result<(), SendError> {
        let _forwarding = self.forwarding();
        let previous = {
            let streams = self.streams();
            if self.tx.is_full() {
                return Err(SendError::Full(VoiceCommand::AddVoice {
                    id,
                    voice,
                    stretch,
                }));
            }
            streams
                .values()
                .find(|e| e.addr.is_some() && e.addr == addr)
                .map_or(LoopSetting::Off, |e| e.loop_)
        };
        let forwarded = forward_loop(self.butler.as_ref(), addr, voice.play.loop_, &self.counters);
        if !forwarded {
            voice.play.loop_ = LoopSetting::Off;
        }
        let loop_ = voice.play.loop_;
        let mut streams = self.streams();
        match self.queue(VoiceCommand::AddVoice { id, voice, stretch }, None) {
            Ok(()) => {
                streams.insert(id, StreamEntry { addr, loop_ });
                Ok(())
            }
            Err(e) => {
                drop(streams);
                if forwarded {
                    self.roll_back(addr, previous);
                }
                Err(e)
            }
        }
    }

    /// Queues `cmd`, the loop command setting `setting` on slot `id`, once
    /// the butler has taken the loop when `id` is a streaming voice; drops it
    /// (counted) when the forward cannot be made.
    fn send_loop(
        &self,
        id: SlotId,
        setting: LoopSetting,
        cmd: VoiceCommand,
    ) -> Result<(), SendError> {
        // An in-memory (or unknown) slot: queued without touching the
        // forwarding lock, so it never waits on the butler.
        {
            let streams = self.streams();
            if !streams.contains_key(&id) {
                return self.queue(cmd, None);
            }
        }
        let _forwarding = self.forwarding();
        let entry = {
            let streams = self.streams();
            // Looked up again: the slot may have changed while this waited.
            let Some(&entry) = streams.get(&id) else {
                return self.queue(cmd, None);
            };
            if self.tx.is_full() {
                return Err(SendError::Full(cmd));
            }
            entry
        };
        if !forward_loop(self.butler.as_ref(), entry.addr, setting, &self.counters) {
            return Ok(());
        }
        let mut streams = self.streams();
        match streams.get_mut(&id) {
            Some(now) if now.addr == entry.addr => match self.queue(cmd, entry.addr) {
                Ok(()) => {
                    now.loop_ = setting;
                    Ok(())
                }
                Err(e) => {
                    drop(streams);
                    self.roll_back(entry.addr, entry.loop_);
                    Err(e)
                }
            },
            // A `Remove`, or an in-memory add at this id, was queued while
            // the butler took the loop: this loop is ordered before it, and
            // the slot it addressed is gone.
            _ => Ok(()),
        }
    }

    /// Puts the butler back on `previous` after a forward whose command
    /// could not be queued. A butler that has exited meanwhile has nothing
    /// left to undo.
    fn roll_back(&self, addr: Option<StreamAddr>, previous: LoopSetting) {
        if let Some((butler, addr)) = self.butler.as_ref().zip(addr) {
            let _ = butler.send_stream_loop(addr.channel_index, addr.stream, previous);
        }
    }

    /// The stream table, locked.
    fn streams(&self) -> std::sync::MutexGuard<'_, HashMap<SlotId, StreamEntry>> {
        self.streams.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The forwarding lock, held.
    fn forwarding(&self) -> std::sync::MutexGuard<'_, ()> {
        self.forwarding
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `cmd` for the drain, with the stream its loop was forwarded to.
    fn queue(&self, cmd: VoiceCommand, forwarded: Option<StreamAddr>) -> Result<(), SendError> {
        self.tx
            .try_send(Queued { cmd, forwarded })
            .map_err(|e| match e {
                TrySendError::Full(q) => SendError::Full(q.cmd),
                TrySendError::Disconnected(q) => SendError::Disconnected(q.cmd),
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
                let unit = stretch::Unit::with_channels(self.rate.load(), self.channels);
                unit.set_stretch_factor(voice.play.stretch);
                unit.set_pitch_cents(voice.play.pitch);
                VoiceCommand::AddVoice {
                    id,
                    voice,
                    stretch: Some(unit),
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
                let unit = stretch::Unit::with_channels(self.rate.load(), self.channels);
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

/// The loop an `UpdateLoop`'s fields describe.
pub(crate) fn loop_setting(
    looping: bool,
    start: SamplePosition,
    end: SamplePosition,
    crossfade_frames: usize,
) -> LoopSetting {
    if looping {
        LoopSetting::On {
            start,
            end,
            crossfade_frames,
        }
    } else {
        LoopSetting::Off
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
        let (_pool, handle) = VoicePool::new().with_handle();

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
        let (pool, handle) = VoicePool::new().with_handle();
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
        let (pool, handle) = VoicePool::new().with_handle();
        drop(pool);

        let err = handle.send(VoiceCommand::Remove(SlotId(7))).unwrap_err();
        match err.into_command() {
            VoiceCommand::Remove(id) => assert_eq!(id, SlotId(7)),
            _ => panic!("the returned command must be the one that was lost"),
        }
    }
}
