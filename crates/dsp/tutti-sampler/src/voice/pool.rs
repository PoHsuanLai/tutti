//! [`VoicePool`] — one graph node that plays every voice on a track.
//!
//! The pool owns its slots, drains [`VoiceCommand`]s each buffer, and sums the
//! results itself. One node per track rather than one per voice plus a dynamic
//! summing unit: adding or removing a voice then costs a queued command instead
//! of a graph edit, which is what keeps voice churn off the commit path.

use std::sync::Arc;

use crate::ports::{Command, Commands};
use crate::stretch;
use crate::{nonempty, MAX_SAMPLER_CHANNELS};

use super::command::{VoiceCommand, VoicePoolHandle, COMMAND_CAPACITY, MAX_RESIDENT_VOICES};
use super::memory_source::LoopSetting;
use super::slot::{stretch_wanted, PlaybackSlot};
use super::types::{SlotId, Voice, VoiceSource};
// Only `playback_of` names it, and that is a `#[cfg(test)]` helper.
#[cfg(test)]
use super::types::Playback;
#[cfg(feature = "bevy")]
use bevy_ecs::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender};
use tutti_core::transport::BeatCursor;
use tutti_core::{
    AudioUnit, BufferMut, BufferRef, ChannelLayout, SampleRate, SignalFrame, Timeline,
};

const VOICE_POOL_ID: u64 = 0x_0000_0000_0000_DA03;

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

/// The graph vertex the track's pool occupies, so a wiring system can name it as
/// a source without searching the `Net`.
///
/// The id, not the unit: the unit belongs to the audio thread, and holding one
/// here would be a second owner of state the graph already owns.
#[cfg(feature = "bevy")]
#[derive(Component, Debug, Clone, Copy)]
pub struct VoicePoolNode(pub tutti_core::dsp::NodeId);

// ---------------------------------------------------------------------------
// Retirement — values the audio thread must not drop.
// ---------------------------------------------------------------------------

/// Something the drain took ownership of and must **not** free in the callback.
///
/// Both variants exist for the same reason: dropping a [`stretch::Unit`] can free
/// its vocoder bank (~192 KB at six channels) when the handle holds the last
/// `Arc` — see that type's `Drop`. Rather than teach two call sites two different
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

// ---------------------------------------------------------------------------
// VoicePool — the AudioUnit.
// ---------------------------------------------------------------------------

/// Every voice on one track, played and summed by a single graph node.
///
/// Zero inputs, [`channels`](Self::channels) outputs. Each block it drains the
/// command queue, checks the transport for a discontinuity, then reads and sums
/// its slots — all of it on the audio thread, and all of it allocation-free
/// provided the control side did its share: [`VoicePoolHandle::send`] builds any
/// stretch filter a command implies, and [`VoicePoolHandle::collect_retired`]
/// frees what the drain hands back.
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
    /// Shared with every clone of this unit — see this type's `Clone`.
    pub(crate) rx: Receiver<VoiceCommand>,

    /// Where removed slots go to be freed, off the audio thread.
    ///
    /// `VoiceCommand::Remove` is handled inside `drain_commands`, which runs
    /// from `tick`/`process` — the audio callback. Dropping the slot there frees
    /// its `stretch::Unit`'s vocoder bank (~192 KB at six channels) in the
    /// callback whenever that handle held the last `Arc`, which is the normal
    /// case: `VoicePoolHandle::send` builds a fresh refcount-1 filter and the
    /// drain moves it in, so no graph commit need ever have cloned it.
    ///
    /// Pushing to a bounded channel instead is lock-free and allocation-free.
    /// The control thread drains it via [`VoicePoolHandle::collect_retired`];
    /// if nobody ever does, the channel fills and the slot is dropped in the
    /// callback — a degraded free, never a leak.
    pub(crate) retired: Sender<Retired>,
    /// The engine rate every slot and its stretch filter is tuned to. Written by
    /// `set_sample_rate` and forwarded to each of them, so a device-rate change
    /// cannot leave a filter tuned to the old one.
    pub(crate) sample_rate: SampleRate,
    /// The clock a placed voice derives its window position from. `None` for a
    /// free-running pool, where every voice plays from its own head.
    pub(crate) transport: Option<Arc<dyn Timeline>>,
    /// Typed butler write handle. `Some` on the live path (threaded in from the
    /// [`DiskStreamer`](crate::DiskStreamer)); `None` for tests / detached / offline
    /// readers with no live butler. Used by the drain to forward *streaming*
    /// loop ops (`Command::Loop`) — loop is butler-owned and not reachable from
    /// the reader itself. Cloning it is cheap (a `Sender` + an `Arc` map).
    pub(crate) butler: Option<Commands>,

    /// Output width — this node's `outputs()`, fixed at construction.
    ///
    /// Declared rather than inferred from the voices it holds: this unit is built
    /// on track creation, *before* any voice exists, and `Net` edges are wired
    /// against `outputs()`. A width that followed its contents would re-arity a
    /// live graph node the moment a voice landed.
    pub(crate) channels: ChannelLayout,

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
    pub(crate) cursor: Option<BeatCursor>,
}

// Hand-rolled: `voices` holds non-`Debug` `PlaybackSlot`s (each wraps a sampler +
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
        Self::from_parts_with_channels(rx, transport, butler, ChannelLayout::STEREO)
    }

    fn from_parts_with_channels(
        rx: Receiver<VoiceCommand>,
        transport: Option<Arc<dyn Timeline>>,
        butler: Option<Commands>,
        channels: impl Into<ChannelLayout>,
    ) -> Self {
        // Detached pools and clones get a dead retirement channel: a full
        // `bounded(0)` never accepts, so `Remove` falls back to dropping in
        // place. That is correct for an offline render, which owns its voices
        // outright and has no control thread waiting to collect.
        let (retired, _) = bounded(0);
        Self {
            retired,
            // Reserved, not empty: `AddVoice` is drained inside `tick`/`process`,
            // so a `push` that grows this vector is a reallocation in the audio
            // callback. `MAX_RESIDENT_VOICES` is the point past which a track
            // stops being allocation-free; beyond it the push still works, it
            // just costs one grow.
            voices: Vec::with_capacity(MAX_RESIDENT_VOICES),
            rx,
            sample_rate: SampleRate::from(44100.0),
            cursor: transport
                .as_ref()
                .map(|t| BeatCursor::new(Arc::clone(t), 44100.0)),
            transport,
            butler,
            channels: nonempty(channels.into()),
        }
    }

    /// Output width — this node's `outputs()`, as a [`ChannelLayout`] rather
    /// than a bare count. Fixed for the pool's lifetime.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// Build a free-running stereo pool and its control handle.
    ///
    /// No transport and no butler: voices play from their own heads, and a
    /// streaming voice's loop command is dropped with a warning because loop is
    /// butler-owned. For a pool on the timeline use
    /// [`with_transport`](Self::with_transport); for one at a non-stereo width,
    /// [`with_channels`](Self::with_channels).
    pub fn new() -> (Self, VoicePoolHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let (retired_tx, retired) = bounded(MAX_RESIDENT_VOICES);
        let handle = VoicePoolHandle {
            tx,
            retired,
            channels: ChannelLayout::STEREO,
            sample_rate: SampleRate::SR_44K1,
        };
        let mut unit = Self::from_parts(rx, None, None);
        unit.retired = retired_tx;
        (unit, handle)
    }

    /// Build a stereo pool on `transport` and its control handle.
    ///
    /// `butler` is the typed write handle streaming voices need: without it a
    /// disk-backed voice still plays, but its loop commands are dropped with a
    /// warning, because the loop-start fadein head lives on the butler side and
    /// the reader cannot reach it.
    pub fn with_transport(
        transport: Arc<dyn Timeline>,
        butler: Option<Commands>,
    ) -> (Self, VoicePoolHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let (retired_tx, retired) = bounded(MAX_RESIDENT_VOICES);
        let handle = VoicePoolHandle {
            tx,
            retired,
            channels: ChannelLayout::STEREO,
            sample_rate: SampleRate::SR_44K1,
        };
        let mut unit = Self::from_parts(rx, Some(transport), butler);
        unit.retired = retired_tx;
        (unit, handle)
    }

    /// As [`with_transport`](Self::with_transport), at an explicit output width.
    ///
    /// Each slot's stretch unit is built at this width too, so a wide voice is
    /// not truncated on the stretch path.
    pub fn with_channels(
        transport: Option<Arc<dyn Timeline>>,
        butler: Option<Commands>,
        channels: impl Into<ChannelLayout>,
    ) -> (Self, VoicePoolHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let (retired_tx, retired) = bounded(MAX_RESIDENT_VOICES);
        let mut unit = Self::from_parts_with_channels(rx, transport, butler, channels);
        unit.retired = retired_tx;
        // The handle mirrors the reader's width/rate so `send` can build a
        // stretch filter that matches it, on the control thread.
        let handle = VoicePoolHandle {
            tx,
            retired,
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
    pub(crate) fn playback_of(&self, id: SlotId) -> Option<&Playback> {
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
    /// [`VoiceNode::replace_transport`](super::node::VoiceNode::replace_transport)
    /// so both transport-aware nodes rebind the same way in the render's
    /// isolation pass.
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

    fn slot_mut(&mut self, id: SlotId) -> Option<&mut PlaybackSlot> {
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
    ///   `SetStreamLoop` / `Off`→`ClearStreamLoop`). Originating the forward here
    ///   is what lets a host speak one `VoiceCommand` for both tiers instead of
    ///   forking on the tier itself.
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
                sampler.set_loop_setting(setting);
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
    /// caller already built.
    ///
    /// **The audio-thread-safe form**: the drain uses it so the callback only
    /// MOVES a filter rather than constructing one. `None` leaves the slot
    /// without a filter, which is correct both for a voice that does not stretch
    /// and (transiently) for one whose filter has not arrived yet — the hot paths
    /// then read the source dry rather than silencing it.
    pub fn insert_voice_with_stretch(
        &mut self,
        id: SlotId,
        voice: Voice,
        stretch: Option<stretch::Unit>,
    ) {
        self.insert_voice_inner(id, voice, stretch);
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
    /// **Allocates when the voice stretches** — control-thread callers only. The
    /// audio-thread drain goes through
    /// [`insert_voice_with_stretch`](Self::insert_voice_with_stretch) instead,
    /// where the filter arrives pre-built. Either way the work sits on the COLD
    /// command drain, above the per-sample loop, so the loop's butler send never
    /// touches the hot path.
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
                    // freeing a `stretch::Unit` that holds the last `Arc` frees its
                    // vocoder bank. Same treatment as `Remove` above.
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
    fn clone(&self) -> Self {
        // `AudioUnit: DynClone`, so the graph (fundsp `Net`) clones this unit on
        // commit (frontend↔backend mem-swap) and may clone it on realloc. The
        // clone therefore MUST keep receiving the commands the ECS handle's
        // `Sender` still feeds — hence the *same* `Receiver` rather than a
        // fresh, dead channel. `crossbeam` delivers each message to
        // exactly one receiver, and the live graph only ever ticks one instance
        // at a time, so there is no double-drain.
        //
        // The offline region render does NOT rely on this: it replaces each
        // reader node with a fresh [`Self::detached`] (channel-less) reader in
        // its Prepare step, so a render clone never shares this `Receiver` while
        // being ticked on a worker thread.
        Self {
            // Dead, like the command `Receiver` beside it: a clone must not hand
            // slots back to the live pool's control thread. A full `bounded(0)`
            // never accepts, so its `Remove`s free in place — correct for a
            // render clone, which owns its voices outright.
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
        // Boundary: `AudioUnit::outputs` is a fixed fundsp trait signature.
        self.channels.count() as usize
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
        // The cursor clones by sharing its cells (so a clone-on-commit does
        // not restart playback): left in place, the clone's `process` would
        // store `last_beat` and its `set_sample_rate` the rate into the live
        // pool's cursor. `rebind_offline` seats a fresh one on the render's
        // transport, as `VoiceNode::isolate` drops its own.
        self.cursor = None;
    }

    /// Seat the render's transport, so voices inserted afterwards are built
    /// against it. The data-carrying half `isolate` defers to; see
    /// [`replace_transport`](Self::replace_transport).
    fn rebind_offline(&mut self, transport: &tutti_core::transport::OfflineTransport) {
        self.replace_transport(transport.timeline());
        // A cursor of its own, on the render's transport: `isolate` dropped
        // the shared one, and seek detection must watch the timeline the
        // render advances.
        self.cursor = Some(BeatCursor::new(transport.timeline(), self.sample_rate));
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate;
        if let Some(cursor) = &mut self.cursor {
            cursor.set_sample_rate(sample_rate.get());
        }
        for slot in &mut self.voices {
            slot.voice
                .source
                .as_audio_unit_mut()
                .set_sample_rate(sample_rate);
            slot.sample_rate = sample_rate;
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

        // Stride derived once, above the per-slot loop.
        let n = (self.channels.count() as usize)
            .min(output.len())
            .min(MAX_SAMPLER_CHANNELS);
        if n == 0 {
            return;
        }
        output[..n].fill(0.0);

        // Each slot reads its ONE voice via the shared
        // `PlaybackSlot::tick_frame_into` (the same per-variant `VoiceSource` match
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

        // Stride derived once per block, above the loops.
        let n = (self.channels.count() as usize)
            .min(output.channels())
            .min(MAX_SAMPLER_CHANNELS);
        for c in 0..n {
            for i in 0..size {
                output.set_f32(c, i, 0.0);
            }
        }

        // Each slot accumulates its ONE voice via the shared
        // `PlaybackSlot::process_into` (the same per-variant `VoiceSource` match a
        // standalone `VoiceNode` uses).
        for slot in &mut self.voices {
            slot.process_into(size, n, output);
        }
    }

    audio_unit_boilerplate!(id = VOICE_POOL_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        // Boundary: `SignalFrame::new` is a fundsp signature.
        SignalFrame::new(self.channels.count() as usize)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>() + self.voices.len() * std::mem::size_of::<PlaybackSlot>()
    }

    /// Size each resident stretch filter's block scratch.
    ///
    /// `stretch::Unit::clone` deliberately leaves that scratch empty — it is
    /// per-block, so copying it per graph commit was pure waste — and this is
    /// the hook that restores it. Forwarding is **required**, not an
    /// optimization: without it a cloned pool reaches the audio thread with
    /// unsized scratch, and `process` has to allocate in the callback to avoid
    /// rendering silence.
    fn allocate(&mut self) {
        for slot in &mut self.voices {
            if let Some(s) = slot.stretch.as_mut() {
                s.allocate();
            }
        }
    }
}
