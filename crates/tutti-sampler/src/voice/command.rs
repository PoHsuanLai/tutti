//! The control plane of the voice nodes: the handles a host steers them
//! through, and the commands a pool's handle queues for its audio thread.
//!
//! A [`VoicePoolHandle`] queues a command per call, drained inside the audio
//! callback, so every command is built control-side, allocation included: the
//! handle is where the stretch filter gets constructed for a voice that needs
//! one, precisely so the callback never has to. A [`VoiceNodeHandle`] steers
//! its one voice and owns the disk [`Stream`] that feeds it.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use crate::streamer::{Stream, StreamId};
use crate::stretch;

use super::disk_voice::DiskVoice;
use super::loop_span::Loop;
use super::memory_source::{MemorySource, VoiceWindow};
use super::pool::{DrainCounters, PoolDrops, Retired};
use super::slot::{stretch_values_want_filter, stretch_wanted};
use super::types::{Direction, Playback, SlotId, Voice, VoiceSource};
use crossbeam_channel::{Receiver, Sender, TrySendError};
use tutti_codec::Wave;
use tutti_graph::ParamSet;
use tutti_rt::ArcLend;
use tutti_types::{
    Amplitude, AtomicSampleRate, Cents, ChannelLayout, Param, PlaybackRate, SampleRate,
    StretchFactor,
};

/// A node's sample rate, shared with its handle: the node stores it when it
/// is prepared (control thread), the handle reads it to build a stretch
/// filter at that rate (control thread). A scalar: no audio-thread read.
#[derive(Clone, Debug)]
pub(crate) struct SharedRate(Arc<AtomicSampleRate>);

impl SharedRate {
    pub(crate) fn new(rate: SampleRate) -> Self {
        Self(Arc::new(AtomicSampleRate::new(rate)))
    }

    pub(crate) fn store(&self, rate: SampleRate) {
        self.0.store(rate, Ordering::Relaxed);
    }

    pub(crate) fn load(&self) -> SampleRate {
        self.0.load(Ordering::Relaxed)
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

/// One control-plane message from a [`VoicePoolHandle`] to its pool's audio
/// thread.
///
/// Drained inside the audio callback, so every variant is built and
/// allocated control-side: that is why the two stretch-carrying variants
/// have the filter handed to them rather than constructing one. Each is one
/// of the handle's methods.
pub(crate) enum VoiceCommand {
    /// Add a voice by handing the reader a fully-formed [`Voice`]: a
    /// [`VoiceSource`] (in-memory `MemorySource` or
    /// streaming `DiskVoice`) plus its
    /// [`Playback`] control-intent record. The drain
    /// builds the slot and applies the full `Playback` per-tier by reusing the
    /// same cold-path appliers the update commands use (`VoiceSource::apply_*` +
    /// `apply_loop`) — no allocation or I/O on the hot path, since the source is
    /// built entirely on the ECS/streamer side before the send.
    ///
    /// One add command for both tiers; the handle's two adds
    /// ([`VoicePoolHandle::add`], which takes a memory source, and
    /// [`VoicePoolHandle::add_streaming`], which takes the stream beside the
    /// disk voice) build it, so a streaming voice never arrives without the
    /// stream that steers its loop and direction.
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
        /// buffers per channel). the handle fills it in
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
    /// Point an in-memory slot at a different wave, keeping its placement,
    /// gain and loop.
    ///
    /// Ignored, and counted
    /// ([`PoolDrops::wave_swaps_ignored`]),
    /// for a streaming slot: swapping a disk source
    /// means re-registering the streamer stream on a different file, which is a
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
    /// Move the voice's timeline window: a [`VoiceWindow`], so not a scalar
    /// param.
    UpdatePlacement {
        /// The slot to move.
        id: SlotId,
        /// Where the voice starts on the timeline and how long it plays.
        window: VoiceWindow,
    },
    /// Set the voice's playback gain.
    UpdateGain {
        /// The slot to set.
        id: SlotId,
        /// Linear [`Amplitude`], not [`Db`](tutti_types::Db) — the conversion is
        /// the host's.
        gain: Amplitude,
    },
    /// Set the voice's varispeed. Both tiers carry speed in-unit: in-memory
    /// stores it on the `MemorySource`, streaming writes the stream's cells,
    /// which the streamer also reads to size its fills, so one command reaches
    /// both.
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
    /// the drain; streaming is the stream's, which owns the loop-start
    /// fadein head the reader cannot reach: [`VoicePoolHandle::send`] queues
    /// the command and sets the loop on the voice's [`Stream`] on the calling
    /// thread. A streaming voice the handle holds no stream for (added without
    /// one) is not set: the loss is counted ([`PoolDrops`]) and the intent is
    /// not recorded — a `Playback` claiming a loop the stream never set would
    /// be replayed as real on the next insert. The drain records a streaming
    /// slot's loop only when the handle named that slot's own stream
    /// ([`PoolDrops::loops_not_forwarded`] otherwise).
    UpdateLoop {
        /// The slot to set.
        id: SlotId,
        /// The loop to play; `None` clears it.
        setting: Option<Loop>,
    },
    /// Play the voice forwards or backwards.
    ///
    /// One command reaches both tiers, which is why it is not folded into
    /// [`UpdateSpeed`](Self::UpdateSpeed): in-memory drives a reversed index in
    /// the hot read, streaming is the stream's direction
    /// ([`Stream::set_direction`]), which [`VoicePoolHandle::send`] sets on
    /// the voice's stream beside queueing the record.
    UpdateReverse {
        /// The slot to set.
        id: SlotId,
        /// [`Direction::Forward`] or [`Direction::Reverse`].
        direction: Direction,
    },
    /// Set the voice's time-stretch factor and pitch shift — the two controls
    /// that change duration and pitch independently, unlike
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
        /// Unboxed, unlike [`AddVoice::stretch`](Self::AddVoice::stretch).
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
            Self::UpdatePlacement { id, window } => f
                .debug_struct("UpdatePlacement")
                .field("id", id)
                .field("window", window)
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
            Self::UpdateLoop { id, setting } => f
                .debug_struct("UpdateLoop")
                .field("id", id)
                .field("setting", setting)
                .finish(),
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
/// `forwarded` is the stream the handle sets a loop or direction command's
/// setting on (it holds that stream, and sets the setting on it beside
/// queueing the command): `None` for every other command, and for a loop or
/// direction it holds no stream for. The drain records a streaming slot's loop
/// or direction only when this names that slot's stream, so whether a setting
/// reached a stream is decided from the command itself, never from what the
/// handle believed the slot held. Only [`VoicePoolHandle::send`] builds one.
pub(crate) struct Queued {
    pub(crate) cmd: VoiceCommand,
    pub(crate) forwarded: Option<StreamId>,
}

// ---------------------------------------------------------------------------
// Handle — held by ECS systems, sends commands to the audio-thread unit.
// ---------------------------------------------------------------------------

/// A command never reached the pool, and the work it carried is gone.
///
/// Modelled on `tutti_transport::QueueFull`: the two failures are told apart
/// because a caller acts on them differently.
///
/// A dropped add is a note that never sounds; a dropped remove is a voice
/// that never stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, thiserror::Error)]
#[non_exhaustive]
pub enum SendError {
    /// The 64-slot command queue was full. Transient: the audio thread drains
    /// it every block, so a caller can back off and retry.
    #[error("voice command queue full; the command was dropped")]
    Full,
    /// The [`VoicePool`](super::pool::VoicePool) has been dropped, so nothing
    /// will ever drain the queue again.
    ///
    /// Permanent: every subsequent call fails the same way, so a caller that
    /// sees this should stop rather than retry. Reporting it is what makes a
    /// dead pool distinguishable from a working one.
    #[error("voice pool is gone; the command was dropped")]
    Disconnected,
}

impl SendError {
    /// Whether this failure is permanent: no later call can succeed either.
    ///
    /// The distinction worth acting on: `Full` means "try again", `Disconnected`
    /// means "stop trying".
    pub fn is_disconnected(self) -> bool {
        matches!(self, Self::Disconnected)
    }
}

/// The controls of a standalone [`VoiceNode`](super::node::VoiceNode): what
/// its `IntoNode` hands back. Its gain, a scalar cell the node reads once per
/// block, its window, loop, speed and play state, published values the node
/// reads once per block, and, for a voice that streams, the disk [`Stream`]
/// that feeds it.
///
/// # An owned handle, and why it is owned
///
/// A host keeps this where the node's control thread is: it is the node's
/// cross-thread control surface (the audio thread reads what it publishes),
/// and it is `Clone` so a UI and an automation thread can both hold one. It
/// owns the disk [`Stream`] because a stream ends when its handle drops, and a
/// stream held apart from the voice is one a caller can drop by accident and
/// silence the voice: here the stream lives exactly as long as the last
/// clone of the handle, which the session's graph keeps for as long as the
/// node is in it. A stream is control-thread only (its drop sends on a
/// bounded queue), so it is never the node's: the node reads the ring, this
/// steers it.
///
/// # Why a separate handle rather than reusing [`VoicePoolHandle`]
///
/// A pool addresses its voices by [`SlotId`] and hands retired slots back for
/// the control thread to free. A `VoiceNode` holds exactly one voice for its
/// whole life: there is no id to address and nothing is ever retired, so both of
/// those fields would be dead weight that every call site has to supply a
/// meaningless value for. The controls it takes are also a strict subset: an
/// add or a remove has no meaning for a node that *is* its voice.
///
/// So its methods name the voice implicitly, and there is no command queue at
/// all: a window, a loop and a speed are values only the latest of which
/// matters, so each is published rather than sent, and nothing a caller does
/// here can hand the node a payload (a boxed `Voice`, a wave, a stretch
/// filter) to drop on the audio thread.
///
/// ```compile_fail
/// # use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoiceSource};
/// # use tutti_codec::Wave;
/// # use std::sync::Arc;
/// # let wave = Arc::new(Wave::from_samples(tutti_types::SampleRate::SR_48K, &[0.0; 64]));
/// # let voice = Voice {
/// #     source: VoiceSource::Memory(MemorySource::new(wave.clone()).expect("a placeable wave")),
/// #     play: Playback::default(),
/// # };
/// let (_node, handle) = VoiceNode::new(voice).with_handle();
/// // A standalone voice node has no door for a pool's wave swap.
/// let _ = handle.replace_wave(tutti_sampler::SlotId(0), wave);
/// ```
///
/// The same setup with a control it does take compiles:
///
/// ```
/// # use tutti_sampler::{MemorySource, Playback, Voice, VoiceNode, VoiceSource, VoiceWindow};
/// # use tutti_codec::Wave;
/// # use std::sync::Arc;
/// # let wave = Arc::new(Wave::from_samples(tutti_types::SampleRate::SR_48K, &[0.0; 64]));
/// # let voice = Voice {
/// #     source: VoiceSource::Memory(MemorySource::new(wave.clone()).expect("a placeable wave")),
/// #     play: Playback::default(),
/// # };
/// let (_node, handle) = VoiceNode::new(voice).with_handle();
/// handle.set_window(VoiceWindow::from(tutti_types::Beat::new(1.0)));
/// ```
#[derive(Clone, Debug)]
pub struct VoiceNodeHandle {
    /// What the handle publishes besides the gain, shared with the node
    /// (which reads it once per block) and with its fork source (which reads
    /// it at fork time).
    pub(crate) steering: SteeringCell,
    /// The latest steering, which the setters edit under the lock and
    /// publish: two handles racing never lose each other's field.
    latest: Arc<Mutex<Steering>>,
    /// The voice's gain: the cell the node reads once per block.
    pub(crate) gain: Param<Amplitude>,
    /// The gain by address ([`UnitParam::Volume`](tutti_types::UnitParam)),
    /// for a host that sets params by address.
    pub(crate) params: ParamSet,
    /// The stream that feeds a disk voice, with its loop set through it.
    stream: Option<Arc<Mutex<Stream>>>,
}

/// What a [`VoiceNode`](super::node::VoiceNode)'s handle publishes besides
/// the gain, latest wins. A field is `None` until the handle first sets it,
/// and the node keeps what it was built with until then.
///
/// The handle publishes (control thread); the node lends the cell once per
/// block, and applies what differs from what it last applied; the fork
/// source reads [`ArcLend::current`] when a fork is taken, so a clip moved
/// after the insert exports where it was moved to.
///
/// Latest-wins loses nothing a queue would have delivered: applying each
/// field is a plain overwrite (no seek, no re-arm), so applying every
/// intermediate value in one block ends in the same state as applying the
/// last, and nothing else rides with it to be ordered against.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct Steering {
    pub(crate) window: Option<VoiceWindow>,
    /// The memory tier's loop; a disk voice's is its stream's.
    pub(crate) loop_: Option<Option<Loop>>,
    pub(crate) speed: Option<PlaybackRate>,
    /// Whether the handle has stopped the voice.
    pub(crate) stopped: bool,
}

pub(crate) type SteeringCell = Arc<ArcLend<Steering>>;

impl VoiceNodeHandle {
    pub(crate) fn new(
        steering: SteeringCell,
        gain: Param<Amplitude>,
        params: ParamSet,
        stream: Option<Stream>,
    ) -> Self {
        Self {
            latest: Arc::new(Mutex::new(*steering.current())),
            steering,
            gain,
            params,
            stream: stream.map(|s| Arc::new(Mutex::new(s))),
        }
    }

    /// Edits the steering under the lock and publishes the result. Control
    /// thread: publishing allocates the new value and frees the one it
    /// replaces, here, so the node never does either.
    fn steer(&self, edit: impl FnOnce(&mut Steering)) {
        let mut latest = self.latest.lock().unwrap_or_else(PoisonError::into_inner);
        edit(&mut latest);
        self.steering.publish(*latest);
    }

    /// Moves the voice's timeline window, from the node's next block on.
    ///
    /// A window is a [`Beat`](tutti_types::Beat) (`f64`) plus an optional
    /// duration: two values, and a precision the transport cannot afford to
    /// lose, so it is no scalar param (an `f32` beat position loses whole
    /// frames past ~2²⁴). It is published to a cell the node reads once per
    /// block: never lost and never queued, and only the latest window is
    /// heard (a window set twice between two blocks plays the second). A fork
    /// of the node taken after this call starts from it.
    pub fn set_window(&self, window: VoiceWindow) {
        self.steer(|s| s.window = Some(window));
    }

    /// Sets the voice's loop, or clears it with `None`, from the node's next
    /// block on.
    ///
    /// A disk voice's loop is its [`Stream`]'s, which this handle owns, so it
    /// is set there, and waits while the streamer's queue is full (a stream's
    /// commands do: control thread, never from a graph node).
    ///
    /// # Errors
    ///
    /// [`StreamerGone`](crate::StreamerGone): the streamer thread behind a
    /// disk voice has exited, and the loop is as it was. Permanent. A memory
    /// voice never fails.
    #[must_use = "a discarded loop is a loop that never plays"]
    pub fn set_loop(&self, setting: Option<Loop>) -> Result<(), crate::StreamerGone> {
        match &self.stream {
            Some(stream) => stream
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .set_loop(setting),
            None => {
                self.steer(|s| s.loop_ = Some(setting));
                Ok(())
            }
        }
    }

    /// The loop as last set through this handle: a disk voice's is the
    /// stream's request (what the streamer applied is the stream's
    /// [`status`](Stream::status)); a memory voice's is the published one, or
    /// `None` until one is set (the node plays the loop it was built with
    /// until then).
    pub fn loop_(&self) -> Option<Loop> {
        match &self.stream {
            Some(stream) => stream
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .loop_(),
            None => self
                .latest
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .loop_
                .flatten(),
        }
    }

    /// What the streamer reports of a disk voice's stream (whether it is
    /// still opening, live or ended, and the fault that ended it); `None` for
    /// a memory voice, which has no stream.
    #[must_use]
    pub fn status(&self) -> Option<crate::StreamStatus> {
        self.stream
            .as_ref()
            .map(|s| s.lock().unwrap_or_else(PoisonError::into_inner).status())
    }

    /// Sets the voice's varispeed (pitch follows the rate), from the node's
    /// next block on, on either tier.
    pub fn set_speed(&self, speed: PlaybackRate) {
        self.steer(|s| s.speed = Some(speed));
    }

    /// Resumes a voice [`stop`](Self::stop)ped: the voice reads and sounds
    /// again from the node's next block. A voice is playing until stopped.
    pub fn play(&self) {
        self.steer(|s| s.stopped = false);
    }

    /// Stops the voice: it is silent from the node's next block and reads
    /// nothing, until [`play`](Self::play). A voice on the transport resumes
    /// where the transport is; a free-running one, where it stopped.
    pub fn stop(&self) {
        self.steer(|s| s.stopped = true);
    }

    /// Whether the voice is playing (not [`stop`](Self::stop)ped).
    pub fn is_playing(&self) -> bool {
        !self
            .latest
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .stopped
    }

    /// Sets the voice's gain, from the node's next block on; a NaN or infinite gain
    /// is silence. A write to a
    /// shared cell: never lost, never queued.
    pub fn set_gain(&self, gain: Amplitude) {
        self.params.set(tutti_types::UnitParam::Volume, gain.get());
    }

    /// The voice's gain, as last set.
    pub fn gain(&self) -> Amplitude {
        self.gain.load()
    }

    /// The node's params by address: its gain, as
    /// [`UnitParam::Volume`](tutti_types::UnitParam::Volume).
    pub fn params(&self) -> &ParamSet {
        &self.params
    }
}

/// The control-thread end of a [`VoicePool`](super::pool::VoicePool)'s command
/// channel, and the collection point for what the audio thread retires.
///
/// Held by the host (an ECS system, typically) and cloned freely — the sender is
/// multi-producer. It mirrors the reader's width, and shares the rate the
/// reader was last prepared at, so `send` can build a stretch
/// filter that matches, on this thread. It holds the [`Stream`] of every
/// streaming voice added through [`add_streaming`](Self::add_streaming), so
/// `send` sets a streaming voice's loop and direction on its stream itself: the
/// pool's drain has no stream to set them on, and a stream cannot live in a
/// voice (its drop sends on a bounded queue, which the audio thread must not
/// do). A stream ends when its voice is removed.
///
/// # Ids the drain refused
///
/// An add the drain refuses at the voice cap
/// ([`PoolDrops::voices_refused`]) still leaves its stream in the handle's table,
/// until a `Remove` or another add at that id:
/// a host that gives every note a fresh id should `Remove` the ids it sees
/// refused (they come back through
/// [`collect_retired`](Self::collect_retired)), or the table grows by one
/// stream per refused streaming add. A loop sent for such an id reaches a
/// stream nothing plays, and the drain finds no slot to record it on.
#[derive(Clone, Debug)]
pub struct VoicePoolHandle {
    /// Sends into the reader's bounded command queue.
    pub(crate) tx: Sender<Queued>,
    /// Slots the audio thread has removed and handed back to be freed here.
    ///
    /// See [`VoicePool`](super::pool::VoicePool)'s `retired`. Draining this is what actually moves the
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
    /// The stream of every streaming voice this handle (or a clone) added with
    /// [`add_streaming`](Self::add_streaming), by slot; a slot holding a memory
    /// voice, or a streaming one added without a stream, has no entry.
    ///
    /// It decides only where `send` sets a loop or a direction, never whether
    /// the drain records one: a voice inserted with
    /// [`insert_voice`](super::pool::VoicePool::insert_voice) after the handle
    /// was made is not in it, and the drain then refuses that slot's loops
    /// ([`PoolDrops::loops_not_forwarded`]).
    pub(crate) streams: SlotStreams,
}

/// [`VoicePoolHandle::streams`]: the table is locked for a lookup or for
/// queueing a command beside changing it, never across a send to the streamer;
/// each stream has a lock of its own, held across the send, so two commands for
/// one slot reach its stream and the pool in one order and a command for another
/// slot (a remove, an in-memory add) never waits on the streamer. Control
/// threads only.
pub(crate) type SlotStreams = Arc<Mutex<HashMap<SlotId, Arc<Mutex<Stream>>>>>;

/// A streaming add that did not reach the pool: why, and the stream, which the
/// handle takes only once the add is queued. Its loop and direction are as they
/// were before the call.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct StreamingAddError {
    /// Why the add was refused, with the command (the voice) it carried.
    pub error: SendError,
    /// The stream that was to steer the voice, handed back.
    pub stream: Stream,
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

    /// Adds an in-memory voice: `source` played as `play` says (its gain,
    /// speed, direction, loop, stretch and pitch; the loop in `play` is the
    /// voice's, replacing any the source holds). An add at an id already
    /// resident replaces that slot.
    ///
    /// A voice that streams from disk is added with
    /// [`add_streaming`](Self::add_streaming), which takes the [`Stream`]
    /// that steers its loop and direction: this takes only what needs no
    /// stream, so a disk voice cannot be added without one.
    ///
    /// ```compile_fail
    /// # use tutti_sampler::{DiskVoice, Playback, SlotId, VoicePool};
    /// # fn add(voice: DiskVoice) {
    /// let (_pool, handle) = VoicePool::new().with_handle();
    /// // A disk voice has no door here: its loop and direction are its stream's.
    /// let _ = handle.add(SlotId(0), voice, Playback::default());
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// [`SendError`]: the add did not reach the pool, and the voice is
    /// dropped with it.
    #[must_use = "a dropped add is a note that never sounds"]
    pub fn add(&self, id: SlotId, source: MemorySource, play: Playback) -> Result<(), SendError> {
        self.send(VoiceCommand::AddVoice {
            id,
            voice: Box::new(Voice {
                source: VoiceSource::Memory(source),
                play,
            }),
            stretch: None,
        })
    }

    /// Removes the voice at `id` (and ends its stream, if it streams). The
    /// slot is handed back to be freed by
    /// [`collect_retired`](Self::collect_retired); an id with no voice is
    /// ignored by the pool.
    ///
    /// # Errors
    ///
    /// [`SendError`]: a dropped remove is a voice that never stops.
    #[must_use = "a dropped remove is a voice that never stops"]
    pub fn remove(&self, id: SlotId) -> Result<(), SendError> {
        self.send(VoiceCommand::Remove(id))
    }

    /// Points the in-memory voice at `id` at a different wave, keeping its
    /// window, gain and loop. A streaming voice is not changed (counted:
    /// [`PoolDrops::wave_swaps_ignored`]): changing a disk source is a new
    /// [`add_streaming`](Self::add_streaming) on a stream opened for the other
    /// file.
    ///
    /// The wave the pool lets go of is freed by
    /// [`collect_retired`](Self::collect_retired), never in the callback.
    ///
    /// # Errors
    ///
    /// [`SendError`], as [`add`](Self::add).
    #[must_use = "a dropped command changes nothing"]
    pub fn replace_wave(&self, id: SlotId, wave: Arc<Wave>) -> Result<(), SendError> {
        self.send(VoiceCommand::ReplaceWave { id, wave })
    }

    /// Moves the voice at `id` on the timeline to `window`.
    ///
    /// # Errors
    ///
    /// [`SendError`], as [`add`](Self::add).
    #[must_use = "a dropped command changes nothing"]
    pub fn set_window(&self, id: SlotId, window: VoiceWindow) -> Result<(), SendError> {
        self.send(VoiceCommand::UpdatePlacement { id, window })
    }

    /// Sets the voice's gain, linear; a NaN or infinite gain is silence.
    ///
    /// # Errors
    ///
    /// [`SendError`], as [`add`](Self::add).
    #[must_use = "a dropped command changes nothing"]
    pub fn set_gain(&self, id: SlotId, gain: Amplitude) -> Result<(), SendError> {
        self.send(VoiceCommand::UpdateGain { id, gain })
    }

    /// Sets the voice's varispeed (pitch follows the rate), on either tier.
    ///
    /// # Errors
    ///
    /// [`SendError`], as [`add`](Self::add).
    #[must_use = "a dropped command changes nothing"]
    pub fn set_speed(&self, id: SlotId, speed: PlaybackRate) -> Result<(), SendError> {
        self.send(VoiceCommand::UpdateSpeed { id, speed })
    }

    /// Sets the voice's loop, or clears it with `None`; on either tier.
    ///
    /// A streaming voice's loop is set on its [`Stream`], which this handle
    /// holds ([`add_streaming`](Self::add_streaming)): the queue is tried
    /// first, so a full queue refuses the call before the stream hears of it
    /// and `Err` leaves both as they were. A stream that cannot take it (its
    /// streamer has exited: permanent) is counted
    /// ([`PoolDrops::loops_streamer_gone`]) and the voice's record is put back
    /// to what the stream has; the call returns `Ok`.
    ///
    /// # Blocking
    ///
    /// Setting a streaming voice's loop waits while the streamer's queue is
    /// full, as a [`Stream`]'s commands do: as long as a stalled streamer
    /// stalls, and for good on a streamer nobody drains (a controller never
    /// started, or a `DiskStreamer::manual` stepped on this thread). While it
    /// waits, another thread's command for *that voice* through a clone of
    /// this handle waits with it, so its stream and the pool see them in one
    /// order. Every other command never waits on another thread's stream.
    ///
    /// # Errors
    ///
    /// [`SendError`], as [`add`](Self::add).
    #[must_use = "a dropped command changes nothing"]
    pub fn set_loop(&self, id: SlotId, setting: Option<Loop>) -> Result<(), SendError> {
        self.send_loop(id, setting)
    }

    /// Plays the voice forwards or backwards. A streaming voice's direction
    /// is its stream's, set as [`set_loop`](Self::set_loop) sets a loop
    /// (counted: [`PoolDrops::directions_dropped`]).
    ///
    /// # Errors
    ///
    /// [`SendError`], as [`add`](Self::add).
    #[must_use = "a dropped command changes nothing"]
    pub fn set_direction(&self, id: SlotId, direction: Direction) -> Result<(), SendError> {
        self.send_reverse(id, direction)
    }

    /// Sets the voice's time-stretch factor and pitch shift: the two controls
    /// that change duration and pitch independently, unlike
    /// [`set_speed`](Self::set_speed). The stretch filter a voice needs is
    /// built here, on the calling thread.
    ///
    /// # Errors
    ///
    /// [`SendError`], as [`add`](Self::add).
    #[must_use = "a dropped command changes nothing"]
    pub fn set_stretch(
        &self,
        id: SlotId,
        factor: StretchFactor,
        pitch: Cents,
    ) -> Result<(), SendError> {
        self.send(VoiceCommand::UpdateStretch {
            id,
            stretch_factor: factor,
            pitch_cents: pitch,
            stretch: None,
        })
    }

    /// Queues a command, doing any allocation it implies here, on the calling
    /// (control) thread.
    ///
    /// This is the one chokepoint every command passes through, which makes it the
    /// right place to keep the audio thread clean: `drain_commands` runs from
    /// `process`, so anything expensive left for the drain is an allocation
    /// in the callback. That means materialising the stretch filter for an
    /// `AddVoice` or an `UpdateStretch` that needs one.
    ///
    /// `Ok` means only that the command was *queued*: the pool applies it on the
    /// audio thread, and that outcome is not available synchronously.
    pub(crate) fn send(&self, cmd: VoiceCommand) -> Result<(), SendError> {
        match self.prepare(cmd) {
            VoiceCommand::AddVoice { id, voice, stretch } => {
                let displaced = {
                    let mut streams = self.streams();
                    self.queue(VoiceCommand::AddVoice { id, voice, stretch }, None)?;
                    streams.remove(&id)
                };
                // The slot this replaced held a stream of its own, if any: it
                // ends (a send), outside the table's lock.
                drop(displaced);
                Ok(())
            }
            VoiceCommand::Remove(id) => {
                let displaced = {
                    let mut streams = self.streams();
                    self.queue(VoiceCommand::Remove(id), None)?;
                    streams.remove(&id)
                };
                drop(displaced);
                Ok(())
            }
            VoiceCommand::UpdateLoop { id, setting } => self.send_loop(id, setting),
            VoiceCommand::UpdateReverse { id, direction } => self.send_reverse(id, direction),
            cmd => self.queue(cmd, None),
        }
    }

    /// Adds a streaming voice, and takes the [`Stream`] that feeds it: from now
    /// on the voice's loop and direction are set on that stream, and the stream
    /// ends when the voice is removed or replaced ([`remove`](Self::remove), or
    /// an add at the same id). `voice` is the one built from the stream's own
    /// reader.
    ///
    /// `play`'s loop and direction are set on the stream first, if they differ
    /// from what it has, and the add is queued only after: a loop the stream
    /// could not take is dropped from the add and counted
    /// ([`PoolDrops::loops_streamer_gone`]), as a direction is
    /// ([`PoolDrops::directions_dropped`]), so the voice's `Playback` never
    /// claims one its stream was not told. A stretch filter the voice needs is
    /// built here.
    ///
    /// Control thread. Setting the loop waits while the streamer's queue is
    /// full ([`set_loop`](Self::set_loop), "Blocking").
    ///
    /// # Errors
    ///
    /// A boxed [`StreamingAddError`] when the add did not reach the pool: the
    /// [`SendError`] (full or disconnected), and the stream handed back with
    /// its loop and direction as they were before the call.
    pub fn add_streaming(
        &self,
        id: SlotId,
        voice: DiskVoice,
        stream: Stream,
        play: Playback,
    ) -> Result<(), Box<StreamingAddError>> {
        let VoiceCommand::AddVoice { id, voice, stretch } = self.prepare(VoiceCommand::AddVoice {
            id,
            voice: Box::new(Voice {
                source: VoiceSource::Disk(voice),
                play,
            }),
            stretch: None,
        }) else {
            unreachable!("`prepare` returns the command kind it was given")
        };
        self.add_stream_voice(id, voice, stream, stretch)
    }

    /// The queueing half of [`add_streaming`](Self::add_streaming): `voice`
    /// is a streaming voice, its stretch filter already built.
    fn add_stream_voice(
        &self,
        id: SlotId,
        mut voice: Box<Voice>,
        mut stream: Stream,
        stretch: Option<stretch::Unit>,
    ) -> Result<(), Box<StreamingAddError>> {
        if self.tx.is_full() {
            let error = SendError::Full;
            return Err(Box::new(StreamingAddError { error, stream }));
        }
        // What the stream had, to put back if the add cannot be queued.
        let (was_loop, was_direction) = (stream.loop_(), stream.direction());
        if voice.play.loop_ != was_loop && stream.set_loop(voice.play.loop_).is_err() {
            DrainCounters::bump(&self.counters.loops_streamer_gone);
            voice.play.loop_ = was_loop;
        }
        if voice.play.direction != was_direction
            && stream.set_direction(voice.play.direction).is_err()
        {
            DrainCounters::bump(&self.counters.directions_dropped);
            voice.play.direction = was_direction;
        }
        // The slot holds what the stream was given (or kept): a loop it could
        // not take is not claimed.
        let queued = {
            let mut streams = self.streams();
            let cmd = VoiceCommand::AddVoice { id, voice, stretch };
            match self.queue(cmd, None) {
                Ok(()) => Ok(streams.insert(id, Arc::new(Mutex::new(stream)))),
                Err(error) => Err((error, stream)),
            }
        };
        match queued {
            // The slot this replaced held a stream of its own, if any: it ends
            // (a send), outside the table's lock.
            Ok(displaced) => {
                drop(displaced);
                Ok(())
            }
            // Another thread filled the queue meanwhile: the stream goes back
            // as it came (best effort: a streamer that is gone has nothing
            // left to undo).
            Err((error, mut stream)) => {
                if stream.loop_() != was_loop {
                    let _ = stream.set_loop(was_loop);
                }
                if stream.direction() != was_direction {
                    let _ = stream.set_direction(was_direction);
                }
                Err(Box::new(StreamingAddError { error, stream }))
            }
        }
    }

    /// Queues the loop command setting `setting` on slot `id`, and sets it on
    /// the slot's stream when the handle holds one. The queue first: a full one
    /// refuses the command with nothing changed; a stream that cannot take it
    /// (its streamer is gone) is counted and the voice's record is put back.
    fn send_loop(&self, id: SlotId, setting: Option<Loop>) -> Result<(), SendError> {
        let cmd = VoiceCommand::UpdateLoop { id, setting };
        let Some(entry) = self.stream_of(id) else {
            // An in-memory (or unknown) slot: nothing to set it on.
            return self.queue(cmd, None);
        };
        let mut stream = entry.lock().unwrap_or_else(PoisonError::into_inner);
        let previous = stream.loop_();
        self.queue(cmd, Some(stream.id()))?;
        if stream.set_loop(setting).is_err() {
            DrainCounters::bump(&self.counters.loops_streamer_gone);
            // The one undo: the voice's record goes back to the loop the stream
            // has. A full queue leaves it; a streamer that is gone plays
            // nothing either way.
            let _ = self.queue(
                VoiceCommand::UpdateLoop {
                    id,
                    setting: previous,
                },
                Some(stream.id()),
            );
        }
        Ok(())
    }

    /// [`send_loop`](Self::send_loop) for the direction.
    fn send_reverse(&self, id: SlotId, direction: Direction) -> Result<(), SendError> {
        let cmd = VoiceCommand::UpdateReverse { id, direction };
        let Some(entry) = self.stream_of(id) else {
            // An in-memory slot reverses its own index; a streaming slot
            // without a stream here is counted by the drain.
            return self.queue(cmd, None);
        };
        let mut stream = entry.lock().unwrap_or_else(PoisonError::into_inner);
        let previous = stream.direction();
        self.queue(cmd, Some(stream.id()))?;
        if stream.set_direction(direction).is_err() {
            DrainCounters::bump(&self.counters.directions_dropped);
            let _ = self.queue(
                VoiceCommand::UpdateReverse {
                    id,
                    direction: previous,
                },
                Some(stream.id()),
            );
        }
        Ok(())
    }

    /// The stream the handle holds for slot `id`, if any.
    pub(crate) fn stream_of(&self, id: SlotId) -> Option<Arc<Mutex<Stream>>> {
        self.streams().get(&id).cloned()
    }

    /// The stream table, locked.
    fn streams(&self) -> std::sync::MutexGuard<'_, HashMap<SlotId, Arc<Mutex<Stream>>>> {
        self.streams.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `cmd` for the drain, with the stream its loop was set on.
    fn queue(&self, cmd: VoiceCommand, forwarded: Option<StreamId>) -> Result<(), SendError> {
        self.tx
            .try_send(Queued { cmd, forwarded })
            .map_err(|e| match e {
                TrySendError::Full(_) => SendError::Full,
                TrySendError::Disconnected(_) => SendError::Disconnected,
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

/// Frees what the control side retired and no block holds, without publishing
/// (the placement windows a replacement left behind); control thread, from the owner's tick.
impl tutti_rt::Reclaim for VoiceNodeHandle {
    fn reclaim(&self) -> usize {
        self.steering.reclaim()
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
        assert_eq!(err, SendError::Full);
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
}
