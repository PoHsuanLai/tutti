//! [`VoicePool`] — one graph node that plays every voice on a track.
//!
//! The pool owns its slots, drains [`VoiceCommand`]s each buffer, and sums the
//! results itself. One node per track rather than one per voice plus a dynamic
//! summing unit: adding or removing a voice then costs a queued command instead
//! of a graph edit, which is what keeps voice churn off the commit path.

use crate::butler::RegionId;
use crate::ports::Commands;
use crate::stretch;
use crate::{nonempty, MAX_SAMPLER_CHANNELS};

use super::clock::{BlockClock, Clock};
use super::command::{
    loop_setting, Queued, SharedRate, StreamEntry, VoiceCommand, VoicePoolHandle, COMMAND_CAPACITY,
    MAX_RESIDENT_VOICES,
};
use super::memory_source::LoopSetting;
use super::slot::{stretch_wanted, PlaybackSlot};
use super::types::{SlotId, Voice, VoiceSource};
use crate::lanes::{BlockScratch, UNPREPARED_FRAMES};
use std::collections::HashMap;
// Only `playback_of` names it, and that is a `#[cfg(test)]` helper.
#[cfg(test)]
use super::types::Playback;
#[cfg(feature = "bevy")]
use bevy_ecs::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tutti_core::{ChannelLayout, SampleRate, Tail};
use tutti_graph::{
    Cx, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare, Shape,
    Status,
};
use tutti_io::Wave;

// ---------------------------------------------------------------------------
// ECS components — live on the track entity.
// ---------------------------------------------------------------------------

/// The control-thread handle to a track's pool, parked on the track entity so a
/// system that has the entity can queue commands without a side registry.
///
/// Not `Clone` as a component even though the handle is: one owner per track,
/// and a system that needs a second sender clones the inner
/// [`VoicePoolHandle`].
#[cfg(feature = "bevy")]
#[derive(Component, Debug)]
pub struct VoicePoolRef(pub VoicePoolHandle);

/// The graph key the track's pool occupies, so a wiring system can name it as
/// a source without searching the graph.
///
/// The key, not the unit: the unit belongs to the audio thread, and holding one
/// here would be a second owner of state the graph already owns.
#[cfg(feature = "bevy")]
#[derive(Component, Debug, Clone, Copy)]
pub struct VoicePoolNode(pub tutti_core::NodeKey);

// ---------------------------------------------------------------------------
// Retirement — values the audio thread must not drop.
// ---------------------------------------------------------------------------

/// Something the drain took ownership of and must **not** free in the callback.
///
/// Every variant exists for the same reason: dropping it can free a large
/// allocation — a [`stretch::Unit`]'s vocoders (~100 KB per channel), or a
/// decoded [`Wave`] whose last reference it is. Rather than teach each call
/// site its own evasion, everything the drain needs to shed goes down one
/// channel and is freed by [`VoicePoolHandle::collect_retired`] on the
/// control thread.
///
/// **A full channel** (the control thread is not collecting) or a
/// disconnected one degrades to dropping the value in the drain: a free on
/// the audio thread, counted ([`PoolDrops::freed_in_callback`]), and no
/// other effect. That is the documented policy of
/// [`VoicePoolHandle::collect_retired`]. Parking the overflow in a spare slot
/// instead would only move the free one value later, since the channel is
/// already the bound.
///
/// `pub(crate)`, not `pub`: [`PlaybackSlot`] is crate-private, and the retirement
/// channel is an internal thread-handoff detail. Callers only ever see the count
/// from [`VoicePoolHandle::collect_retired`], never the values.
///
/// # Why the payloads are never read
///
/// No field is ever inspected, and that is the design: the *value* is the
/// payload, and receiving it is what frees it. `collect_retired` pulls each one
/// and lets it fall out of scope, on the control thread. So `dead_code` is
/// correct that nothing reads them, and wrong that they are dead — deleting
/// any field would move the deallocation back into the audio callback.
///
/// The variants are also very different sizes (a `PlaybackSlot` dwarfs a bare
/// filter), which normally argues for boxing the large one. Not here: this value
/// exists to cross a thread boundary and be dropped, so a `Box` would add an
/// allocation on one side and a free on the other — the exact cost being avoided.
/// The channel is `bounded(MAX_RESIDENT_VOICES)`, so the waste is one slot-sized
/// element per queue entry, bounded and never in the callback's path.
#[allow(
    dead_code,
    clippy::large_enum_variant,
    reason = "the payloads exist to be received and dropped on the control thread, never read; boxing the large variant would re-add the audio-thread free this type exists to avoid"
)]
pub(crate) enum Retired {
    /// A slot removed by `VoiceCommand::Remove`, filter and all.
    Slot(PlaybackSlot),
    /// A stretch filter the sender built for an `UpdateStretch` that turned out
    /// not to need it, because the slot already had one.
    ///
    /// The sender cannot know whether a filter is resident — that is audio-thread
    /// state — so it builds whenever the new values ask for stretching and accepts
    /// that some arrivals are redundant. Handing the spare back here is what keeps
    /// that trade free of an audio-thread free.
    ///
    /// Unboxed: boxing it would mean allocating on the control thread and
    /// *deallocating* on the audio one, which is the hazard this type exists to
    /// avoid.
    Stretch(stretch::Unit),
    /// A wave a `ReplaceWave` let go of: the one a memory slot swapped out,
    /// or the new one when no memory slot took it.
    ///
    /// An `Arc`, so retiring it frees nothing unless this was the last
    /// reference, and then it frees the whole decoded buffer: the reason it
    /// is retired at all.
    Wave(Arc<Wave>),
    /// A voice an `AddVoice` brought to a pool already holding
    /// `MAX_RESIDENT_VOICES` slots, refused whole: its box, and the filter
    /// the sender built for it.
    ///
    /// Taking it would grow the slot vector (a reallocation in the
    /// callback), and dropping it would free the box, the source and the
    /// filter there. Counted as [`PoolDrops::voices_refused`].
    Refused {
        voice: Box<Voice>,
        stretch: Option<stretch::Unit>,
    },
}

/// What a [`VoicePool`] and its handle dropped, refused or freed where they
/// should not have, since the pool was built: a reading a host polls
/// ([`VoicePoolHandle::drops`]), never a log line from the callback.
///
/// Each is a count of commands (or values) and only ever grows. The
/// `loops_butler_gone` and `loops_no_butler` counts are taken where a
/// streaming voice's loop is forwarded to the butler, on the control thread
/// ([`VoicePoolHandle::send`], [`VoicePool::insert_voice`]); the rest by the
/// drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct PoolDrops {
    /// Streaming loops (set or cleared) whose forward found the butler
    /// thread exited. The loop is not recorded in the voice's `Playback`.
    pub loops_butler_gone: u64,
    /// Streaming loops turned on (`LoopSetting::On`) for a voice with no
    /// butler to forward to: a pool built without one, or a voice with no
    /// butler channel. Clearing a loop there is a no-op, not a drop.
    pub loops_no_butler: u64,
    /// Loop commands (set or cleared) the drain found aimed at a streaming
    /// slot whose stream the handle had not forwarded them to: a voice
    /// inserted with [`VoicePool::insert_voice`] after the handle was made,
    /// which the handle does not know. Not recorded, since the butler was
    /// not told.
    pub loops_not_forwarded: u64,
    /// `ReplaceWave` commands aimed at a streaming voice, which changes
    /// source by a fresh `AddVoice` instead.
    pub wave_swaps_ignored: u64,
    /// `AddVoice` commands for a new slot refused because the pool already
    /// held `MAX_RESIDENT_VOICES` (64) slots. The voice goes back through
    /// the retirement channel to be freed by
    /// [`collect_retired`](VoicePoolHandle::collect_retired).
    pub voices_refused: u64,
    /// Values the drain had to free in the callback because the retirement
    /// channel was full: the control thread was not calling
    /// [`collect_retired`](VoicePoolHandle::collect_retired) often enough.
    pub freed_in_callback: u64,
}

/// The atomics behind [`PoolDrops`], shared by a pool and its handle. Bumped
/// with a relaxed add (by the drain on the audio thread, and by a loop
/// forward on the control thread); read on the control thread.
#[derive(Debug, Default)]
pub(crate) struct DrainCounters {
    loops_butler_gone: AtomicU64,
    loops_no_butler: AtomicU64,
    loops_not_forwarded: AtomicU64,
    wave_swaps_ignored: AtomicU64,
    voices_refused: AtomicU64,
    freed_in_callback: AtomicU64,
}

impl DrainCounters {
    fn bump(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> PoolDrops {
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        PoolDrops {
            loops_butler_gone: load(&self.loops_butler_gone),
            loops_no_butler: load(&self.loops_no_butler),
            loops_not_forwarded: load(&self.loops_not_forwarded),
            wave_swaps_ignored: load(&self.wave_swaps_ignored),
            voices_refused: load(&self.voices_refused),
            freed_in_callback: load(&self.freed_in_callback),
        }
    }
}

/// The butler stream a streaming voice reads: its channel, and the stream
/// itself, which the butler checks so a loop set on a voice whose stream has
/// since stopped or been replaced on the channel is not applied to another.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamAddr {
    pub(crate) channel_index: usize,
    pub(crate) stream: RegionId,
}

/// Where a voice's loop is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LoopRoute {
    /// On the voice's in-memory source, by the drain.
    Memory,
    /// On its butler stream, forwarded from the control thread; `None` for a
    /// streaming voice with no butler channel.
    Stream(Option<StreamAddr>),
}

impl Voice {
    /// Where this voice's loop is applied.
    pub(crate) fn loop_route(&self) -> LoopRoute {
        match &self.source {
            VoiceSource::Memory(_) => LoopRoute::Memory,
            VoiceSource::Disk(disk) => {
                LoopRoute::Stream(self.channel_index.map(|channel_index| StreamAddr {
                    channel_index,
                    stream: disk.stream(),
                }))
            }
        }
    }
}

/// Forwards a streaming voice's loop to `butler` (blocking: **control
/// thread**), and says whether the pool may record it: only once the butler
/// has taken it. A forward that cannot be made is counted in `counters`,
/// except an `Off` with no butler to tell, which loses nothing (the butler
/// was never given a loop to clear).
pub(crate) fn forward_loop(
    butler: Option<&Commands>,
    addr: Option<StreamAddr>,
    setting: LoopSetting,
    counters: &DrainCounters,
) -> bool {
    let Some((butler, addr)) = butler.zip(addr) else {
        if matches!(setting, LoopSetting::On { .. }) {
            DrainCounters::bump(&counters.loops_no_butler);
        }
        return false;
    };
    if butler
        .send_stream_loop(addr.channel_index, addr.stream, setting)
        .is_err()
    {
        DrainCounters::bump(&counters.loops_butler_gone);
        return false;
    }
    true
}

/// A [`VoicePool`] asked for more channels than the sampler reads
/// ([`MAX_SAMPLER_CHANNELS`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error(
    "a voice pool {} channels wide: the sampler reads at most {MAX_SAMPLER_CHANNELS}",
    channels.count()
)]
pub struct PoolTooWide {
    /// The width asked for.
    pub channels: ChannelLayout,
}

// ---------------------------------------------------------------------------
// VoicePool — the graph node.
// ---------------------------------------------------------------------------

/// Every voice on one track, played and summed by a single graph node.
///
/// Zero inputs, [`channels`](Self::channels) outputs. Each block it drains the
/// command queue, reads the transport from the block's `Env`, then reads and
/// sums its slots — all of it on the audio thread, and all of it
/// allocation-free provided the control side did its share:
/// [`VoicePoolHandle::send`] builds any stretch filter a command implies, and
/// [`VoicePoolHandle::collect_retired`] frees what the drain hands back.
///
/// # As a graph node
///
/// A `tutti_graph::Node`. Its controls ([`IntoNode`]) are its
/// [`VoicePoolHandle`]: the command queue it drains at the top of each block
/// (a bounded lock-free queue the node owns the receiving end of), and the
/// retirement channel back. Voices placed on the timeline read the transport
/// from each block's `Env`, frame-exact at their windows' edges.
///
/// **A fork of a pool is an empty pool** at its width, with no command
/// queue and no butler: the voices a pool holds arrived through its queue on
/// the audio thread, where no control-side copy of them exists to fork. A
/// host that renders a track's clips offline rebuilds them in a pool of its
/// own ([`insert_voice`](Self::insert_voice)) before inserting it.
///
/// Both source tiers live here side by side, as the `Memory` / `Disk` arms of
/// [`VoiceSource`]; the slot's read forks on that enum rather than on a trait,
/// which is what keeps a per-tier difference visible at the call site.
pub struct VoicePool {
    /// The resident slots, one per voice, summed in order. Reserved to
    /// `MAX_RESIDENT_VOICES` at construction, and the audio-thread
    /// `AddVoice` drain never grows it: an add for a new slot once it is full
    /// is refused and retired (`Retired::Refused`). Only a control-thread
    /// [`insert_voice`](Self::insert_voice) may grow it, and the drain then
    /// fills the larger capacity before refusing.
    pub(crate) voices: Vec<PlaybackSlot>,
    /// Commands from the control thread, drained at the top of every block.
    /// This pool's own: a dead one until [`into_parts`](IntoNode::into_parts)
    /// (or [`with_handle`](Self::with_handle)) hands out the sending end; a
    /// clone gets a dead one — see this type's `Clone`.
    pub(crate) rx: Receiver<Queued>,

    /// Where removed slots go to be freed, off the audio thread.
    ///
    /// `VoiceCommand::Remove` is handled inside `drain_commands`, which runs
    /// from `tick`/`process` — the audio callback. Dropping the slot there frees
    /// its `stretch::Unit`'s vocoders (~100 KB per channel), which the unit
    /// owns, in the callback.
    ///
    /// Pushing to a bounded channel instead is lock-free and allocation-free.
    /// The control thread drains it via [`VoicePoolHandle::collect_retired`];
    /// if nobody ever does, the channel fills and the slot is dropped in the
    /// callback — a degraded free, never a leak, counted as
    /// [`PoolDrops::freed_in_callback`].
    pub(crate) retired: Sender<Retired>,
    /// What the drain dropped or refused, shared with the handle
    /// ([`VoicePoolHandle::drops`]).
    pub(crate) counters: Arc<DrainCounters>,
    /// The engine rate every slot and its stretch filter is tuned to. Written by
    /// `prepare` and forwarded to each of them, so a device-rate change
    /// cannot leave a filter tuned to the old one.
    pub(crate) sample_rate: SampleRate,
    /// The rate, shared with the handle, so a filter the handle builds for a
    /// command is built at the rate the pool runs at.
    pub(crate) rate: SharedRate,
    /// Typed butler write handle, until a handle is taken. `Some` on the
    /// live path (threaded in from the [`DiskStreamer`](crate::DiskStreamer));
    /// `None` for tests / detached / offline readers with no live butler. A
    /// streaming voice's loop is butler-owned and not reachable from the
    /// reader, so it is forwarded there (`Command::Loop`), always from the
    /// control thread: by [`insert_voice`](Self::insert_voice) while the pool
    /// holds it, then by the handle it moves to
    /// ([`with_handle`](Self::with_handle)). So the node in the graph holds no
    /// butler sender at all.
    pub(crate) butler: Option<Commands>,

    /// Output width — the node's audio outputs, fixed at construction.
    ///
    /// Declared rather than inferred from the voices it holds: this unit is built
    /// on track creation, *before* any voice exists, and edges are wired against
    /// its shape. A width that followed its contents would re-arity a live graph
    /// node the moment a voice landed.
    pub(crate) channels: ChannelLayout,

    /// The transport as the pool's voices read it, kept across blocks so a
    /// jump (a seek, a loop wrap) is seen where it happens and every slot's
    /// buffered audio is flushed there.
    ///
    /// **One clock for the whole reader, not one per slot.** Every slot reads
    /// the same transport, so N clocks would be N chances to disagree about
    /// whether the playhead moved — and a disagreement would flush some slots
    /// and not others, which is worse than flushing none.
    pub(crate) clock: Clock,

    /// The lanes every slot's block read renders through, one voice at a
    /// time (`PlaybackSlot::process_into`). Built with the pool and sized
    /// from `Prepare::max_block` in `prepare`, on the control thread.
    scratch: BlockScratch,
}

// Hand-rolled: `voices` holds non-`Debug` `PlaybackSlot`s (each wraps a sampler +
// stretch DSP). Print the slot count + scalars rather than the slot internals.
impl std::fmt::Debug for VoicePool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoicePool")
            .field("voices", &self.voices.len())
            .field("sample_rate", &self.sample_rate)
            .field("has_butler", &self.butler.is_some())
            .finish_non_exhaustive()
    }
}

impl VoicePool {
    /// A pool with no command queue and no retirement channel yet (both
    /// dead): [`with_handle`](Self::with_handle) or
    /// [`into_parts`](IntoNode::into_parts) makes them.
    fn from_parts(butler: Option<Commands>, channels: impl Into<ChannelLayout>) -> Self {
        // A full `bounded(0)` never accepts, so until a handle exists `Remove`
        // falls back to dropping in place — correct for a pool nothing
        // controls (a fork, which owns its voices outright and has no control
        // thread waiting to collect).
        let (retired, _) = bounded(0);
        let sample_rate = SampleRate::SR_44K1;
        let channels = nonempty(channels.into());
        Self {
            retired,
            counters: Arc::default(),
            // Reserved, not empty: `AddVoice` is drained inside `process`, so
            // a `push` that grows this vector would be a reallocation in the
            // audio callback. The drain refuses an add past the capacity
            // instead (`Retired::Refused`).
            voices: Vec::with_capacity(MAX_RESIDENT_VOICES),
            rx: bounded(0).1,
            sample_rate,
            rate: SharedRate::new(sample_rate),
            butler,
            channels,
            clock: Clock::new(),
            scratch: BlockScratch::new(UNPREPARED_FRAMES, channels.count() as usize),
        }
    }

    /// Output width — the node's audio outputs, as a [`ChannelLayout`]
    /// rather than a bare count. Fixed for the pool's lifetime.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// A stereo pool with no butler: a streaming voice's loop is
    /// dropped and counted ([`PoolDrops::loops_no_butler`]), because loop is
    /// butler-owned. Placed voices
    /// play their windows of the transport; for a pool that forwards loops,
    /// [`with_butler`](Self::with_butler); for one at a non-stereo width,
    /// [`with_channels`](Self::with_channels). Its controls come with it into
    /// the graph ([`IntoNode`]).
    pub fn new() -> Self {
        Self::from_parts(None, ChannelLayout::STEREO)
    }

    /// A stereo pool whose streaming voices' loops are forwarded to `butler`
    /// (by [`insert_voice`](Self::insert_voice) until the pool's handle is
    /// taken, then by the handle, on the control thread) — the typed write
    /// handle streaming voices need:
    /// without it a disk-backed voice still plays, but its loops are dropped
    /// and counted, because the loop-start fadein head lives on the butler
    /// side and the reader cannot reach it.
    pub fn with_butler(butler: Commands) -> Self {
        Self::from_parts(Some(butler), ChannelLayout::STEREO)
    }

    /// A pool at an explicit output width, with an optional butler.
    ///
    /// Each slot's stretch unit is built at this width too, so a wide voice is
    /// not truncated on the stretch path.
    ///
    /// # Errors
    ///
    /// [`PoolTooWide`] past [`MAX_SAMPLER_CHANNELS`]: the pool's read stacks
    /// and lanes are that wide, so a wider pool would declare outputs it never
    /// writes (a caller reading them sees whatever the buffer held).
    pub fn with_channels(
        butler: Option<Commands>,
        channels: impl Into<ChannelLayout>,
    ) -> Result<Self, PoolTooWide> {
        let channels = channels.into();
        if channels.count() as usize > MAX_SAMPLER_CHANNELS {
            return Err(PoolTooWide { channels });
        }
        Ok(Self::from_parts(butler, channels))
    }

    /// This pool with a fresh command queue and retirement channel, and the
    /// handle that drives them: what [`into_parts`](IntoNode::into_parts)
    /// hands the graph and the caller. For a host (or a test) that calls
    /// the node by hand. A handle taken before goes dead
    /// ([`SendError::Disconnected`](super::command::SendError::Disconnected)).
    ///
    /// The pool's butler moves into the handle: from here on a streaming
    /// voice's loop is forwarded by the handle, and the pool (and a second
    /// handle taken from it) has no butler. A streaming voice
    /// [`insert_voice`](Self::insert_voice) adds after this cannot have its
    /// loop set: its `On` is counted ([`PoolDrops::loops_no_butler`]), and
    /// a loop sent for it through the handle reaches the drain unforwarded
    /// ([`PoolDrops::loops_not_forwarded`]). Add voices through the handle.
    pub fn with_handle(mut self) -> (Self, VoicePoolHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let (retired_tx, retired) = bounded(MAX_RESIDENT_VOICES);
        self.rx = rx;
        self.retired = retired_tx;
        // The handle mirrors the reader's width and shares its rate so `send`
        // can build a stretch filter that matches it, on the control thread.
        // It forwards streaming loops to the butler itself, so it starts
        // knowing the stream of every streaming voice already inserted.
        let streams: HashMap<_, _> = self
            .voices
            .iter()
            .filter_map(|slot| match slot.voice.loop_route() {
                LoopRoute::Stream(addr) => Some((
                    slot.id,
                    StreamEntry {
                        addr,
                        loop_: slot.voice.play.loop_,
                    },
                )),
                LoopRoute::Memory => None,
            })
            .collect();
        let handle = VoicePoolHandle {
            tx,
            retired,
            channels: self.channels,
            rate: self.rate.clone(),
            counters: Arc::clone(&self.counters),
            butler: self.butler.take(),
            streams: Arc::new(std::sync::Mutex::new(streams)),
            forwarding: Arc::default(),
        };
        (self, handle)
    }

    /// Number of voice slots currently materialised (drained from the command
    /// queue). Diagnostic / test helper.
    pub fn voice_count(&self) -> usize {
        self.voices.len()
    }

    /// A block longer than the scratch, in pieces as long as it, every slot
    /// per piece: only a pool driven by hand without a `prepare` (its
    /// `UNPREPARED_FRAMES`) or past the `max_block` it was prepared for
    /// reaches this.
    #[cold]
    #[inline(never)]
    fn render_in_pieces(
        &mut self,
        clock: &BlockClock<'_>,
        frames: usize,
        n: usize,
        out: &mut [&mut [f32]],
    ) {
        let piece = self.scratch.frames();
        let mut from = 0;
        while from < frames {
            let to = (from + piece).min(frames);
            for slot in &mut self.voices {
                slot.render_into(clock, from..to, n, &mut self.scratch, out);
            }
            from = to;
        }
    }

    /// Its scratch's frames and width.
    #[cfg(test)]
    pub(crate) fn scratch_size(&self) -> (usize, usize) {
        (self.scratch.frames(), self.scratch.width())
    }

    /// The control intent recorded for a slot. Test helper: lets a test assert
    /// that a dropped command did NOT leave a `Playback` claiming it applied.
    #[cfg(test)]
    pub(crate) fn playback_of(&self, id: SlotId) -> Option<&Playback> {
        self.voices
            .iter()
            .find(|s| s.id == id)
            .map(|s| &s.voice.play)
    }

    /// Drops every voice slot. Control thread (it frees them).
    pub fn clear_voices(&mut self) {
        self.voices.clear();
    }

    fn slot_mut(&mut self, id: SlotId) -> Option<&mut PlaybackSlot> {
        self.voices.iter_mut().find(|s| s.id == id)
    }

    /// Apply a loop setting to a slot, by tier, and record it in the slot's
    /// `Playback`.
    ///
    /// - **In-memory**: primes / clears the loop range on the `MemorySource`
    ///   in-unit (`MemorySource::set_loop_setting`).
    /// - **Streaming**: the loop is butler-owned — `SetStreamLoop` reads the
    ///   loop's fade lead-in off disk and mutates `plan.link.loop_config`,
    ///   neither reachable from the reader — and the control thread has
    ///   already forwarded it ([`VoicePoolHandle::send`],
    ///   [`insert_voice`](Self::insert_voice)), so here it is only recorded.
    ///   The one caller is `insert_voice_inner`, whose voice carries a loop
    ///   only once the butler took it (its sender set it `Off` otherwise), so
    ///   the record never claims a loop the stream was not given. A loop
    ///   command goes through [`apply_queued_loop`](Self::apply_queued_loop).
    ///
    /// Allocation-free and send-free: the drain runs it.
    fn apply_loop(&mut self, id: SlotId, setting: LoopSetting) {
        let Some(slot) = self.voices.iter_mut().find(|s| s.id == id) else {
            return;
        };
        if let VoiceSource::Memory(sampler) = &mut slot.voice.source {
            sampler.set_loop_setting(setting);
        }
        slot.voice.play.loop_ = setting;
    }

    /// A queued loop command's setting, applied as
    /// [`apply_loop`](Self::apply_loop) does, except that a streaming slot
    /// records it only when `forwarded` names that slot's own stream: the
    /// handle forwarded it there before queueing. Otherwise (the handle did
    /// not know the slot's stream) it is counted
    /// ([`PoolDrops::loops_not_forwarded`]) and the slot keeps its loop, so
    /// the decision rests on the command, not on the handle's table. A loop
    /// the butler then ignores, because the stream has since stopped or its
    /// channel been reused, is still recorded: harmless, since the slot no
    /// longer plays from that stream.
    ///
    /// Allocation-free and send-free: the drain runs it.
    fn apply_queued_loop(
        &mut self,
        id: SlotId,
        setting: LoopSetting,
        forwarded: Option<StreamAddr>,
    ) {
        let Some(slot) = self.voices.iter().find(|s| s.id == id) else {
            return;
        };
        if let LoopRoute::Stream(addr) = slot.voice.loop_route() {
            if addr.is_none() || addr != forwarded {
                DrainCounters::bump(&self.counters.loops_not_forwarded);
                return;
            }
        }
        self.apply_loop(id, setting);
    }

    /// As [`insert_voice`](Self::insert_voice), taking a stretch filter the
    /// caller already built. `None` leaves the slot without a filter, which is
    /// correct both for a voice that does not stretch and (transiently) for
    /// one whose filter has not arrived yet — the hot paths then read the
    /// source dry rather than silencing it.
    ///
    /// Boxes the voice (the slot holds it boxed), so control thread only; the
    /// audio-thread drain hands the box it received straight to the slot.
    /// It can wait on the butler as `insert_voice` does
    /// (see its "Blocking" section).
    pub fn insert_voice_with_stretch(
        &mut self,
        id: SlotId,
        mut voice: Voice,
        stretch: Option<stretch::Unit>,
    ) {
        if let LoopRoute::Stream(addr) = voice.loop_route() {
            if !forward_loop(self.butler.as_ref(), addr, voice.play.loop_, &self.counters) {
                voice.play.loop_ = LoopSetting::Off;
            }
        }
        self.insert_voice_inner(id, Box::new(voice), stretch);
    }

    /// Inserts a fully-built [`Voice`] as a new slot and realises its full
    /// `Playback` intent per-tier, building the stretch filter here if the voice
    /// needs one.
    ///
    /// The single path [`VoiceCommand::AddVoice`] funnels through. Public so a
    /// voice-aware caller (the offline region render's `Populate` step) can hand
    /// the reader a `Voice` it built from ECS data — gain / loop / direction /
    /// stretch / pitch carried on `voice.play` — instead of pre-poking a
    /// `MemorySource` before the send.
    ///
    /// # How the intent is realised
    ///
    /// The `Playback` is control-INTENT, and each tier applies it its own way.
    /// Rather than duplicate the tier fork, this replays the exact cold-path
    /// appliers the `Update*` commands use: `VoiceSource::apply_*` (per-tier
    /// match — in-memory stores on the `MemorySource`, streaming forwards to the
    /// shared state the butler ring reads) for gain / speed / direction, and
    /// `apply_loop` for loop (in-memory primes the range in-unit; a streaming
    /// loop is forwarded to the butler here first, then recorded). Stretch and pitch are primed
    /// from `play` when the slot is built.
    ///
    /// # Thread
    ///
    /// **Allocates** (the filter, when the voice stretches, and the slot's box)
    /// — control-thread callers only. The audio-thread drain moves in the
    /// box and the filter the command carries instead, both built by the
    /// sender, and sends nothing to the butler.
    ///
    /// # Blocking
    ///
    /// A streaming voice forwards its loop with the butler's blocking send
    /// (as [`Commands::send`]): while the butler's queue is full, this waits
    /// until there is room or the butler is gone (then the loop is dropped,
    /// counted as [`PoolDrops::loops_butler_gone`], and not recorded). Two
    /// cases make that wait long:
    ///
    /// - **A stalled butler** (on disk I/O, or descheduled): this waits as
    ///   long as the stall does.
    /// - **Nobody draining the queue**: a butler controller that was never
    ///   started, or a `DiskStreamer::manual` (`test-support`) stepped on
    ///   the thread that calls this. The queue then drains only
    ///   when that same thread steps it, so a full queue waits for good.
    ///   Step the streamer (or start it) before inserting a looping voice
    ///   into a full queue.
    ///
    /// Every other voice, and every in-memory one, never waits.
    pub fn insert_voice(&mut self, id: SlotId, voice: Voice) {
        let stretch = stretch_wanted(&voice.play).then(|| {
            let unit = stretch::Unit::with_channels(self.sample_rate, self.channels);
            unit.set_stretch_factor(voice.play.stretch);
            unit.set_pitch_cents(voice.play.pitch);
            unit
        });
        self.insert_voice_with_stretch(id, voice, stretch);
    }

    /// Allocation-free, free-free and send-free, so the drain can run it: the
    /// voice arrives boxed and the box moves into the slot, and a slot this
    /// one replaces goes to the retirement channel rather than being dropped
    /// here. A streaming voice's loop arrives already forwarded (or `Off`);
    /// see [`apply_loop`](Self::apply_loop).
    fn insert_voice_inner(
        &mut self,
        id: SlotId,
        mut voice: Box<Voice>,
        stretch: Option<stretch::Unit>,
    ) {
        if let Some(i) = self.voices.iter().position(|s| s.id == id) {
            let old = self.voices.swap_remove(i);
            self.retire(Retired::Slot(old));
        }
        // Take the loop out, leaving `Off` behind: `apply_loop` needs the slot
        // present to look it up, and applies it (an in-memory loop is primed
        // on the source) as it records it.
        let loop_ = std::mem::replace(&mut voice.play.loop_, LoopSetting::Off);
        let gain = voice.play.gain;
        let speed = voice.play.speed;
        let direction = voice.play.direction;
        // `PlaybackSlot::with_channels` primes the resident stretch unit from
        // `voice.play` (stretch/pitch) — the one heavy step, done here off the
        // hot path. It is built at THIS READER's width: a slot narrower than the
        // reader would truncate on the stretch path only, which no stereo test
        // can observe.
        let mut slot = PlaybackSlot::with_channels(id, voice, self.sample_rate, self.channels);
        slot.stretch = stretch;
        // The source runs at the pool's rate (the memory tier's conversion,
        // the disk tier's step), as `prepare` sets every resident slot's.
        // Allocation-free: a rate and a ratio.
        slot.voice.source.prepare_rate(self.sample_rate);
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

    /// Hands a value the drain let go of to the control thread.
    ///
    /// A full channel (the control thread is not collecting) or a
    /// disconnected one (a pool no handle was taken for) drops it right here
    /// instead: correctness is unaffected, only the thread that pays for the
    /// free, and the count says it happened.
    ///
    /// The channel holds `MAX_RESIDENT_VOICES` (64) values, and one drain
    /// takes at most `COMMAND_CAPACITY` (64) commands, each retiring at most
    /// one value: a control thread that collects once between sends never
    /// overflows it. Refused voices share the bound with every other
    /// retirement, so a host that floods a full pool with adds and never
    /// collects frees the 65th refusal in the callback, counted.
    fn retire(&self, value: Retired) {
        if self.retired.try_send(value).is_err() {
            DrainCounters::bump(&self.counters.freed_in_callback);
        }
    }

    fn drain_commands(&mut self) {
        while let Ok(Queued { cmd, forwarded }) = self.rx.try_recv() {
            match cmd {
                VoiceCommand::AddVoice { id, voice, stretch } => {
                    // A new slot in a full vector would grow it here, in the
                    // callback. Refuse it whole instead (an add at a resident
                    // id replaces in place and always fits).
                    let replaces = self.voices.iter().any(|s| s.id == id);
                    if !replaces && self.voices.len() == self.voices.capacity() {
                        DrainCounters::bump(&self.counters.voices_refused);
                        self.retire(Retired::Refused { voice, stretch });
                        continue;
                    }
                    // The filter arrives prebuilt from `send` (control thread);
                    // this only moves it into the slot.
                    self.insert_voice_inner(id, voice, stretch);
                }
                VoiceCommand::Remove(id) => {
                    // `retain` would DROP the slot here, on the audio thread,
                    // freeing its stretch filter's vocoder bank inside the
                    // callback. Swap it out and hand it to the control thread
                    // instead — a lock-free push, no free.
                    if let Some(i) = self.voices.iter().position(|s| s.id == id) {
                        let slot = self.voices.swap_remove(i);
                        self.retire(Retired::Slot(slot));
                    }
                }
                VoiceCommand::ReplaceWave { id, wave } => {
                    // Whichever wave this arm ends up not keeping — the old
                    // one a memory slot swaps out, or the new one when there
                    // is no memory slot to take it — may hold the last
                    // reference to a decoded buffer, and dropping it here
                    // would free that buffer in the callback. It leaves
                    // through the retirement channel instead.
                    let shed = match self.slot_mut(id).map(|s| &mut s.voice.source) {
                        // Match the tier explicitly rather than calling through
                        // a shared setter: the streaming half has nothing to
                        // do, and a shared setter would make a `ReplaceWave`
                        // aimed at a disk voice do nothing and say nothing.
                        // Swapping a streaming source means re-registering the
                        // butler stream on a different file, which is a
                        // control-thread op issued as a fresh `AddVoice`.
                        Some(VoiceSource::Memory(sampler)) => sampler.replace_wave(wave),
                        Some(VoiceSource::Disk(_)) => {
                            DrainCounters::bump(&self.counters.wave_swaps_ignored);
                            wave
                        }
                        None => wave,
                    };
                    self.retire(Retired::Wave(shed));
                }
                VoiceCommand::UpdatePlacement {
                    id,
                    start_beat,
                    duration_beats,
                } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // The source owns the window: each backend's setter
                        // overwrites it, and the next block's read places the
                        // voice by it.
                        slot.voice
                            .source
                            .apply_placement(start_beat, duration_beats);
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
                        // In-memory stores it on the `MemorySource`; streaming
                        // forwards to the shared `RtState` (the exact speed
                        // effect of the butler's `SetVarispeed`, reachable from
                        // the reader). `apply_speed` covers both, so a host
                        // never forks streaming speed onto a butler command.
                        slot.voice.source.apply_speed(speed);
                        slot.voice.play.speed = speed;
                    }
                }
                VoiceCommand::UpdateLoop {
                    id,
                    looping,
                    loop_start,
                    loop_end,
                    crossfade_frames,
                } => {
                    // A streaming slot's loop reached the butler before this
                    // was queued (`VoicePoolHandle::send`), if `forwarded`
                    // names its stream.
                    let setting = loop_setting(looping, loop_start, loop_end, crossfade_frames);
                    self.apply_queued_loop(id, setting, forwarded);
                }
                VoiceCommand::ClearLoop(id) => {
                    self.apply_queued_loop(id, LoopSetting::Off, forwarded);
                }
                VoiceCommand::UpdateReverse { id, direction } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // In-memory: `voice.play.direction` drives the reversed index
                        // in the hot read (the source-side `set_direction` is a
                        // no-op). Streaming: the source-side `set_direction`
                        // forwards to the shared `RtState` (the direction leg of the
                        // butler's `SetVarispeed`) — `voice.play.direction` is
                        // unused by the ring pull. One command reaches both, so
                        // a host sends reverse ONCE rather than folding it into
                        // a separate butler speed command.
                        slot.voice.play.direction = direction;
                        slot.voice.source.apply_direction(direction);
                    }
                }
                VoiceCommand::UpdateStretch {
                    id,
                    stretch_factor,
                    pitch_cents,
                    stretch,
                } => {
                    // Take the surplus out of `set_stretch` and retire it rather
                    // than letting it drop here: this is the audio callback, and
                    // dropping a `stretch::Unit` frees its vocoders. Same
                    // treatment as `Remove` above.
                    //
                    // `slot_mut` returning `None` (the slot was removed between
                    // send and drain) must retire the filter too — an early
                    // `return`/`continue` here would drop it on this thread.
                    let surplus = match self.slot_mut(id) {
                        Some(slot) => slot.set_stretch(stretch_factor, pitch_cents, stretch),
                        None => stretch,
                    };
                    if let Some(unit) = surplus {
                        self.retire(Retired::Stretch(unit));
                    }
                }
            }
        }
    }
}

impl Clone for VoicePool {
    /// A copy of the voices and settings, with **no command channel** and no
    /// retirement channel of its own: a copy draining the live handle's
    /// commands would steal edits from the audio thread (crossbeam hands each
    /// message to exactly one receiver), so the `Receiver` stays with the
    /// pool that owns it.
    fn clone(&self) -> Self {
        Self {
            // Dead, like the command `Receiver` beside it: a clone must not hand
            // slots back to the live pool's control thread.
            retired: bounded(0).0,
            counters: Arc::default(),
            voices: {
                // Reserved like a new pool's, so a clone's drain refuses at the
                // same count rather than at whatever `collect` sized.
                let mut voices = Vec::with_capacity(MAX_RESIDENT_VOICES.max(self.voices.len()));
                voices.extend(self.voices.iter().map(|s| PlaybackSlot {
                    id: s.id,
                    voice: s.voice.clone(),
                    stretch: s.stretch.clone(),
                    channels: s.channels,
                    sample_rate: s.sample_rate,
                }));
                voices
            },
            rx: bounded(0).1,
            sample_rate: self.sample_rate,
            rate: SharedRate::new(self.sample_rate),
            clock: Clock::new(),
            butler: self.butler.clone(),
            channels: self.channels,
            scratch: BlockScratch::sized_like(&self.scratch),
        }
    }
}

impl Default for VoicePool {
    fn default() -> Self {
        Self::new()
    }
}

impl Node for VoicePool {
    /// No inputs, [`channels`](Self::channels) outputs; a generator (fed
    /// out of band, through its queue), never skipped.
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, self.channels)
            .with_tail(Tail::Unbounded)
            // Its voices arrive while it plays, each a `VoiceNode`'s cost:
            // measured 13 ns per frame and channel at one plain voice, 82 at
            // eight, and one stretched voice alone is 288 (doc 013, "The
            // cost model and coarsening"). The default, stated.
            .with_cost(tutti_graph::Cost::Heavy)
    }

    /// The rate every slot, its source and its stretch filter run at, and
    /// the handle builds filters at; the block scratch sized from
    /// `max_block` (a re-prepare at another size resizes it).
    fn prepare(&mut self, p: &Prepare) {
        let sample_rate = p.sample_rate();
        self.sample_rate = sample_rate;
        self.rate.store(sample_rate);
        self.scratch
            .resize(p.max_block().get(), self.channels.count() as usize);
        for slot in &mut self.voices {
            slot.voice.source.prepare_rate(sample_rate);
            slot.sample_rate = sample_rate;
            if let Some(unit) = &mut slot.stretch {
                unit.set_sample_rate(sample_rate);
            }
        }
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.drain_commands();
        let clock = self.clock.observe(cx.env);
        let frames = io.frames();
        let (_, mut outs) = io.split();
        for ch in outs.iter_mut() {
            ch.fill(0.0);
        }
        // Stride derived once per block, above the loops.
        let n = (self.channels.count() as usize)
            .min(outs.len())
            .min(MAX_SAMPLER_CHANNELS);
        let mut refs: [&mut [f32]; MAX_SAMPLER_CHANNELS] =
            std::array::from_fn(|_| Default::default());
        for (slot, ch) in refs.iter_mut().zip(outs.iter_mut()) {
            *slot = ch;
        }
        // Each slot renders its ONE voice into the pool's lanes and adds them
        // in, a lane at a time, via the shared `PlaybackSlot::render_into`
        // (the same per-variant `VoiceSource` match a standalone `VoiceNode`
        // uses), in one piece: in a graph, `prepare` sized the lanes for
        // every block.
        if frames <= self.scratch.frames() {
            for slot in &mut self.voices {
                slot.render_into(&clock, 0..frames, n, &mut self.scratch, &mut refs[..n]);
            }
        } else {
            self.render_in_pieces(&clock, frames, n, &mut refs[..n]);
        }
        Status::Modified
    }

    /// Every slot's buffered audio flushed, the transport forgotten.
    fn reset(&mut self) {
        for slot in &mut self.voices {
            slot.flush_playhead_state();
        }
        self.clock.reset();
    }
}

/// A pool's fork: an empty pool at its width (see "As a graph node").
struct PoolFork {
    channels: ChannelLayout,
}

impl ForkSource for PoolFork {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(VoicePool::from_parts(
            None,
            self.channels,
        ))))
    }
}

impl IntoNode for VoicePool {
    type Controls = VoicePoolHandle;

    fn into_parts(self) -> NodeParts<VoicePoolHandle> {
        let fork = PoolFork {
            channels: self.channels,
        };
        let (pool, handle) = self.with_handle();
        NodeParts {
            node: Box::new(pool),
            controls: handle,
            fork: Some(Box::new(fork)),
        }
    }
}
