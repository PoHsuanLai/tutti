//! [`VoicePool`] — one graph node that plays every voice on a track.
//!
//! The pool owns its slots, drains [`VoiceCommand`]s each buffer, and sums the
//! results itself. One node per track rather than one per voice plus a dynamic
//! summing unit: adding or removing a voice then costs a queued command instead
//! of a graph edit, which is what keeps voice churn off the commit path.

use crate::lanes::LANE_FRAMES;
use crate::ports::{Command, Commands};
use crate::stretch;
use crate::{nonempty, MAX_SAMPLER_CHANNELS};

use super::clock::Clock;
use super::command::{
    SharedRate, VoiceCommand, VoicePoolHandle, COMMAND_CAPACITY, MAX_RESIDENT_VOICES,
};
use super::memory_source::LoopSetting;
use super::slot::{stretch_wanted, PlaybackSlot};
use super::types::{SlotId, Voice, VoiceSource};
use crate::lanes::BlockScratch;
// Only `playback_of` names it, and that is a `#[cfg(test)]` helper.
#[cfg(test)]
use super::types::Playback;
#[cfg(feature = "bevy")]
use bevy_ecs::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender};
use tutti_core::{AudioUnit as _, ChannelLayout, SampleRate, Tail};
use tutti_graph::{
    Cx, ForkCause, ForkMode, ForkSource, Forked, IntoNode, Io, Node, NodeParts, Prepare, Shape,
    Status,
};

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
/// Both variants exist for the same reason: dropping a [`stretch::Unit`] frees
/// its vocoders (~100 KB per channel), which it owns. Rather than teach two call sites two different
/// evasions, everything the drain needs to shed goes down one channel and is
/// freed by [`VoicePoolHandle::collect_retired`] on the control thread.
///
/// `pub(crate)`, not `pub`: [`PlaybackSlot`] is crate-private, and the retirement
/// channel is an internal thread-handoff detail. Callers only ever see the count
/// from [`VoicePoolHandle::collect_retired`], never the values.
///
/// # Why the payloads are never read
///
/// Neither field is ever inspected, and that is the design: the *value* is the
/// payload, and receiving it is what frees it. `collect_retired` pulls each one
/// and lets it fall out of scope, on the control thread. So `dead_code` is
/// correct that nothing reads them, and wrong that they are dead — deleting
/// either field would move the deallocation back into the audio callback.
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
    /// `MAX_RESIDENT_VOICES` at construction so the audio-thread `AddVoice`
    /// drain does not reallocate.
    pub(crate) voices: Vec<PlaybackSlot>,
    /// Commands from the control thread, drained at the top of every block.
    /// This pool's own: a dead one until [`into_parts`](IntoNode::into_parts)
    /// (or [`with_handle`](Self::with_handle)) hands out the sending end; a
    /// clone gets a dead one — see this type's `Clone`.
    pub(crate) rx: Receiver<VoiceCommand>,

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
    /// callback — a degraded free, never a leak.
    pub(crate) retired: Sender<Retired>,
    /// The engine rate every slot and its stretch filter is tuned to. Written by
    /// `prepare` and forwarded to each of them, so a device-rate change
    /// cannot leave a filter tuned to the old one.
    pub(crate) sample_rate: SampleRate,
    /// The rate, shared with the handle, so a filter the handle builds for a
    /// command is built at the rate the pool runs at.
    pub(crate) rate: SharedRate,
    /// Typed butler write handle. `Some` on the live path (threaded in from the
    /// [`DiskStreamer`](crate::DiskStreamer)); `None` for tests / detached / offline
    /// readers with no live butler. Used by the drain to forward *streaming*
    /// loop ops (`Command::Loop`) — loop is butler-owned and not reachable from
    /// the reader itself. Cloning it is cheap (a `Sender` + an `Arc` map).
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
    /// time (`PlaybackSlot::process_into`). Built with the pool, on the
    /// control thread.
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
        Self {
            retired,
            // Reserved, not empty: `AddVoice` is drained inside `process`, so
            // a `push` that grows this vector is a reallocation in the audio
            // callback. `MAX_RESIDENT_VOICES` is the point past which a track
            // stops being allocation-free; beyond it the push still works, it
            // just costs one grow.
            voices: Vec::with_capacity(MAX_RESIDENT_VOICES),
            rx: bounded(0).1,
            sample_rate,
            rate: SharedRate::new(sample_rate),
            butler,
            channels: nonempty(channels.into()),
            clock: Clock::new(),
            scratch: BlockScratch::new(),
        }
    }

    /// Output width — the node's audio outputs, as a [`ChannelLayout`]
    /// rather than a bare count. Fixed for the pool's lifetime.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// A stereo pool with no butler: a streaming voice's loop command is
    /// dropped with a warning, because loop is butler-owned. Placed voices
    /// play their windows of the transport; for a pool that forwards loops,
    /// [`with_butler`](Self::with_butler); for one at a non-stereo width,
    /// [`with_channels`](Self::with_channels). Its controls come with it into
    /// the graph ([`IntoNode`]).
    pub fn new() -> Self {
        Self::from_parts(None, ChannelLayout::STEREO)
    }

    /// A stereo pool forwarding streaming voices' loop commands to `butler`
    /// — the typed write handle streaming voices need: without it a
    /// disk-backed voice still plays, but its loop commands are dropped with
    /// a warning, because the loop-start fadein head lives on the butler side
    /// and the reader cannot reach it.
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
    pub fn with_handle(mut self) -> (Self, VoicePoolHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let (retired_tx, retired) = bounded(MAX_RESIDENT_VOICES);
        self.rx = rx;
        self.retired = retired_tx;
        // The handle mirrors the reader's width and shares its rate so `send`
        // can build a stretch filter that matches it, on the control thread.
        let handle = VoicePoolHandle {
            tx,
            retired,
            channels: self.channels,
            rate: self.rate.clone(),
        };
        (self, handle)
    }

    /// Number of voice slots currently materialised (drained from the command
    /// queue). Diagnostic / test helper.
    pub fn voice_count(&self) -> usize {
        self.voices.len()
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

    /// Drop every voice slot. Control thread (it frees them).
    pub fn clear_voices(&mut self) {
        self.voices.clear();
    }

    fn slot_mut(&mut self, id: SlotId) -> Option<&mut PlaybackSlot> {
        self.voices.iter_mut().find(|s| s.id == id)
    }

    /// Apply a loop setting to a slot, routing by tier.
    ///
    /// - **In-memory**: primes / clears the loop range on the `MemorySource` in-unit
    ///   (`MemorySource::set_loop_setting`).
    /// - **Streaming**: loop is butler-owned — `SetStreamLoop` reads the loop's
    ///   fade lead-in off disk and mutates `plan.link.loop_config`, neither
    ///   reachable from the reader — so the reader FORWARDS to the butler via the
    ///   typed [`Commands`] handle (`Command::Loop`, which maps `On`→
    ///   `SetStreamLoop` / `Off`→`ClearStreamLoop`). Originating the forward here
    ///   is what lets a host speak one `VoiceCommand` for both tiers instead of
    ///   forking on the tier itself.
    ///
    /// RT-safe: this runs on the COLD command drain (top of `process`, before
    /// the read), so the channel send is fine — it never touches the
    /// per-sample hot path.
    fn apply_loop(&mut self, id: SlotId, setting: LoopSetting) {
        let Some(slot) = self.voices.iter_mut().find(|s| s.id == id) else {
            return;
        };
        match &mut slot.voice.source {
            VoiceSource::Memory(sampler) => {
                sampler.set_loop_setting(setting);
                slot.voice.play.loop_ = setting;
            }
            VoiceSource::Disk(_) => {
                // Streaming loop is butler-owned, so this needs both a butler
                // handle and a registered channel. A reader built without one
                // (`new()`, a fork) has neither.
                let Some((butler, channel_index)) =
                    self.butler.as_ref().zip(slot.voice.channel_index)
                else {
                    // Do NOT record the intent: the butler was never told, and
                    // a `play.loop_` that says "looping" while the stream is
                    // not would make `Playback` lie about the applied state —
                    // which `insert_voice` then replays as if it were real.
                    tracing::warn!(
                        "loop command dropped for slot {id:?}: streaming voice has no butler channel"
                    );
                    return;
                };
                // Same rule as the guard above, now that the send can report:
                // only record the intent if the butler actually received it. A
                // `play.loop_` that says "looping" while the stream is not would
                // make `Playback` lie about the applied state, and
                // `insert_voice` replays that as if it were real.
                if let Err(e) = butler.send(Command::Loop {
                    channel_index,
                    setting,
                }) {
                    tracing::warn!("loop command dropped for slot {id:?}: {e}");
                    return;
                }
                slot.voice.play.loop_ = setting;
            }
        }
    }

    /// As [`insert_voice`](Self::insert_voice), taking a stretch filter the
    /// caller already built. `None` leaves the slot without a filter, which is
    /// correct both for a voice that does not stretch and (transiently) for
    /// one whose filter has not arrived yet — the hot paths then read the
    /// source dry rather than silencing it.
    ///
    /// Boxes the voice (the slot holds it boxed), so control thread only; the
    /// audio-thread drain hands the box it received straight to the slot.
    pub fn insert_voice_with_stretch(
        &mut self,
        id: SlotId,
        voice: Voice,
        stretch: Option<stretch::Unit>,
    ) {
        self.insert_voice_inner(id, Box::new(voice), stretch);
    }

    /// Insert a fully-built [`Voice`] as a new slot and REALISE its full
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
    /// `apply_loop` for loop (in-memory primes the range in-unit, streaming
    /// forwards `Command::Loop` to the butler). Stretch and pitch are primed
    /// from `play` when the slot is built.
    ///
    /// # Thread
    ///
    /// **Allocates** (the filter, when the voice stretches, and the slot's box)
    /// — control-thread callers only. The audio-thread drain moves in the
    /// box and the filter the command carries instead, both built by the
    /// sender. Either way the work sits on the COLD
    /// command drain, above the per-sample loop, so the loop's butler send never
    /// touches the hot path.
    pub fn insert_voice(&mut self, id: SlotId, voice: Voice) {
        let stretch = stretch_wanted(&voice.play).then(|| {
            let unit = stretch::Unit::with_channels(self.sample_rate, self.channels);
            unit.set_stretch_factor(voice.play.stretch);
            unit.set_pitch_cents(voice.play.pitch);
            unit
        });
        self.insert_voice_inner(id, Box::new(voice), stretch);
    }

    /// Allocation-free and free-free, so the drain can run it: the voice
    /// arrives boxed and the box moves into the slot, and a slot this one
    /// replaces goes to the retirement channel rather than being dropped here.
    fn insert_voice_inner(
        &mut self,
        id: SlotId,
        voice: Box<Voice>,
        stretch: Option<stretch::Unit>,
    ) {
        if let Some(i) = self.voices.iter().position(|s| s.id == id) {
            let old = self.voices.swap_remove(i);
            // A full or disconnected channel drops it here instead: only the
            // thread that pays for the free changes.
            let _ = self.retired.try_send(Retired::Slot(old));
        }
        // Split the loop out: `apply_loop` needs the slot present to look it up,
        // and `Playback` moves into the `Voice`. Take the rest by copy first.
        let loop_ = voice.play.loop_;
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

    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                VoiceCommand::AddVoice { id, voice, stretch } => {
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
                        // A full or disconnected channel drops the slot right
                        // here instead: correctness is unaffected, only the
                        // thread that pays for the free.
                        let _ = self.retired.try_send(Retired::Slot(slot));
                    }
                }
                VoiceCommand::ReplaceWave { id, wave } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // Match the tier explicitly rather than calling through
                        // a shared setter: the streaming half has nothing to
                        // do, and a shared setter would make a `ReplaceWave`
                        // aimed at a disk voice do nothing and say nothing.
                        // Swapping a streaming source means re-registering the
                        // butler stream on a different file, which is a
                        // control-thread op issued as a fresh `AddVoice`.
                        match &mut slot.voice.source {
                            VoiceSource::Memory(sampler) => sampler.set_wave(wave),
                            VoiceSource::Disk(_) => {
                                tracing::warn!(
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
                        // The source owns the gate; each backend's
                        // `set_placement` re-arms its own (streaming re-seeks on
                        // the next inside-frame).
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
                    let setting = if looping {
                        LoopSetting::On {
                            start: loop_start,
                            end: loop_end,
                            crossfade_frames,
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
                        let _ = self.retired.try_send(Retired::Stretch(unit));
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
            voices: self
                .voices
                .iter()
                .map(|s| PlaybackSlot {
                    id: s.id,
                    voice: s.voice.clone(),
                    stretch: s.stretch.clone(),
                    channels: s.channels,
                    sample_rate: s.sample_rate,
                })
                .collect(),
            rx: bounded(0).1,
            sample_rate: self.sample_rate,
            rate: SharedRate::new(self.sample_rate),
            clock: Clock::new(),
            butler: self.butler.clone(),
            channels: self.channels,
            scratch: BlockScratch::new(),
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
        Shape::audio(ChannelLayout::EMPTY, self.channels).with_tail(Tail::Unbounded)
    }

    /// The rate every slot, its source and its stretch filter run at, and
    /// the handle builds filters at.
    fn prepare(&mut self, p: &Prepare) {
        let sample_rate = p.sample_rate();
        self.sample_rate = sample_rate;
        self.rate.store(sample_rate);
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
        // uses).
        let mut from = 0;
        while from < frames {
            let to = (from + LANE_FRAMES).min(frames);
            for slot in &mut self.voices {
                slot.render_into(&clock, from..to, n, &mut self.scratch, &mut refs[..n]);
            }
            from = to;
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
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
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
