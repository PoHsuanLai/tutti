//! [`VoicePool`] — one graph node that plays every voice on a track.
//!
//! Replaces the old "one graph node per voice plus a dynamic summing unit"
//! model: the pool owns its slots, drains [`VoiceCommand`]s each buffer, and
//! sums the results itself.

use std::sync::Arc;

use crate::ports::{Command, Commands};
use crate::stretch;
use crate::MAX_SAMPLER_CHANNELS;

use super::command::{VoiceCommand, VoicePoolHandle, COMMAND_CAPACITY, MAX_RESIDENT_VOICES};
use super::memory_source::LoopSetting;
use super::slot::{stretch_wanted, VoiceSlot};
use super::types::{Playback, SlotId, Voice, VoiceSource};
#[cfg(feature = "bevy")]
use bevy_ecs::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender};
use tutti_core::transport::BeatCursor;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SampleRate, SignalFrame, Timeline};

const VOICE_POOL_ID: u64 = 0x_0000_0000_0000_DA03;

// ---------------------------------------------------------------------------
// ECS components — live on the track entity.
// ---------------------------------------------------------------------------

#[cfg(feature = "bevy")]
#[derive(Component, Debug)]
pub struct VoicePoolRef(pub VoicePoolHandle);

#[cfg(feature = "bevy")]
#[derive(Component, Debug, Clone, Copy)]
pub struct VoicePoolNode(pub tutti_core::NodeId);

// ---------------------------------------------------------------------------
// VoicePool — the AudioUnit.
// ---------------------------------------------------------------------------

pub struct VoicePool {
    pub(crate) voices: Vec<VoiceSlot>,
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
    /// callback as before — degrading to today's behaviour rather than leaking.
    pub(crate) retired: Sender<VoiceSlot>,
    pub(crate) sample_rate: SampleRate,
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
    pub(crate) channels: usize,

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

// Hand-rolled: `voices` holds non-`Debug` `VoiceSlot`s (each wraps a sampler +
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
        Self::from_parts_with_channels(rx, transport, butler, 2)
    }

    fn from_parts_with_channels(
        rx: Receiver<VoiceCommand>,
        transport: Option<Arc<dyn Timeline>>,
        butler: Option<Commands>,
        channels: usize,
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
            channels: channels.max(1),
        }
    }

    /// Output width — this node's `outputs()`.
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn new() -> (Self, VoicePoolHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let (retired_tx, retired) = bounded(MAX_RESIDENT_VOICES);
        let handle = VoicePoolHandle {
            tx,
            retired,
            channels: 2,
            sample_rate: SampleRate::from(44100.0),
        };
        let mut unit = Self::from_parts(rx, None, None);
        unit.retired = retired_tx;
        (unit, handle)
    }

    pub fn with_transport(
        transport: Arc<dyn Timeline>,
        butler: Option<Commands>,
    ) -> (Self, VoicePoolHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let (retired_tx, retired) = bounded(MAX_RESIDENT_VOICES);
        let handle = VoicePoolHandle {
            tx,
            retired,
            channels: 2,
            sample_rate: SampleRate::from(44100.0),
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
        channels: usize,
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
    /// [`VoiceNode::replace_transport`] so both transport-aware nodes rebind the
    /// same way in the render's isolation pass.
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

    fn slot_mut(&mut self, id: SlotId) -> Option<&mut VoiceSlot> {
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
    ///   `SetStreamLoop` / `Off`→`ClearStreamLoop`). This is the same command
    ///   dawai-model used to send itself; it now originates here so dawai speaks
    ///   one unified `VoiceCommand` for both tiers.
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
                sampler.set_loop_setting(setting.clone());
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
                butler.send(Command::Loop {
                    channel_index,
                    setting: setting.clone(),
                });
                slot.voice.play.loop_ = setting;
            }
        }
    }

    /// Insert a fully-built [`Voice`] as a new slot and REALISE its full
    /// `Playback` intent per-tier — the single path the [`AddVoice`] command
    /// funnels through. Public so a voice-aware caller (the offline region
    /// render's `Populate` step) can hand the reader a `Voice` it built from
    /// ECS DATA — gain / loop / direction / stretch / pitch carried on
    /// `voice.play` — instead of pre-poking a `MemorySource` before send.
    ///
    /// The `Playback` is control-INTENT; each tier applies it its own way. Rather
    /// than duplicate the tier fork, we replay the exact cold-path appliers the
    /// `Update*` commands use: `VoiceSource::apply_*` (per-tier match — in-memory
    /// stores on the `MemorySource`, streaming forwards to the shared `RtState`)
    /// for gain / speed / direction, and `apply_loop` for loop (in-memory
    /// primes the range in-unit, streaming forwards `Command::Loop` to the
    /// butler). Stretch/pitch are primed by `VoiceSlot::new` from `play`. Runs on
    /// the COLD command drain, so the loop's butler send is RT-safe.
    ///
    /// [`AddVoice`]: VoiceCommand::AddVoice
    /// As [`insert_voice`](Self::insert_voice), taking a stretch filter the
    /// caller already built.
    ///
    /// This is the audio-thread-safe form: the drain uses it so the callback
    /// only MOVES a filter rather than constructing one. `None` leaves the slot
    /// without a filter, which is correct both for a voice that does not stretch
    /// and (transiently) for one whose filter has not arrived yet: the hot paths
    /// then read the source dry rather than silencing it.
    pub fn insert_voice_with_stretch(
        &mut self,
        id: SlotId,
        voice: Voice,
        stretch: Option<stretch::Unit>,
    ) {
        self.insert_voice_inner(id, voice, stretch);
    }

    /// Insert a voice, building the stretch filter here if one is needed.
    ///
    /// **Allocates when the voice stretches** — control-thread callers only.
    /// The audio-thread drain goes through
    /// [`insert_voice_with_stretch`](Self::insert_voice_with_stretch) instead.
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
        let loop_ = voice.play.loop_.clone();
        let gain = voice.play.gain;
        let speed = voice.play.speed;
        let direction = voice.play.direction;
        // `VoiceSlot::with_channels` primes the resident stretch unit from
        // `voice.play` (stretch/pitch) — the one heavy step, done here off the
        // hot path. It is built at THIS READER's width: a slot narrower than the
        // reader would truncate on the stretch path only, which no stereo test
        // can observe.
        let mut slot = VoiceSlot::with_channels(id, voice, self.sample_rate, self.channels);
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
                        // A full or disconnected channel drops here, which is
                        // exactly the old behaviour: correctness is unaffected,
                        // only the thread that pays for the free.
                        let _ = self.retired.try_send(slot);
                    }
                }
                VoiceCommand::ReplaceWave { id, wave } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // Match the tier explicitly rather than calling through
                        // a shared setter whose streaming half was a silent
                        // no-op: a `ReplaceWave` aimed at a disk voice did
                        // nothing and said nothing. Swapping a streaming
                        // source means re-registering the butler stream on a
                        // different file, which is a control-thread op issued
                        // from dawai-model as a fresh `AddVoice`.
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
                        // In-memory stores it on the `MemorySource`; streaming forwards
                        // to the shared `RtState` (the exact speed effect of the
                        // butler's `SetVarispeed`, reachable from the reader). The
                        // `apply_speed` covers both — dawai no longer
                        // forks streaming speed onto a separate butler command.
                        slot.voice.source.apply_speed(speed);
                        slot.voice.play.speed = speed;
                    }
                }
                VoiceCommand::UpdateLoop {
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
                        // dawai sends reverse ONCE, no longer folding it into a
                        // separate butler speed command.
                        slot.voice.play.direction = direction;
                        slot.voice.source.apply_direction(direction);
                    }
                }
                VoiceCommand::UpdateStretch {
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

impl Clone for VoicePool {
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
            // Dead, like the command `Receiver` beside it: a clone must not hand
            // slots back to the live pool's control thread. A full `bounded(0)`
            // never accepts, so its `Remove`s free in place — correct for a
            // render clone, which owns its voices outright.
            retired: bounded(0).0,
            voices: self
                .voices
                .iter()
                .map(|s| VoiceSlot {
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
        self.channels
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
    }

    /// Seat the render's transport, so voices inserted afterwards are built
    /// against it. The data-carrying half `isolate` defers to; see
    /// [`replace_transport`](Self::replace_transport).
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        let Some(transport) = ctx.downcast_ref::<tutti_core::transport::OfflineTransport>() else {
            return;
        };
        self.replace_transport(transport.clone());
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

        let n = self.channels.min(output.len()).min(MAX_SAMPLER_CHANNELS);
        if n == 0 {
            return;
        }
        output[..n].fill(0.0);

        // Each slot reads its ONE voice via the shared
        // `VoiceSlot::tick_frame_into` (the same per-variant `VoiceSource` match
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

        let n = self
            .channels
            .min(output.channels())
            .min(MAX_SAMPLER_CHANNELS);
        for c in 0..n {
            for i in 0..size {
                output.set_f32(c, i, 0.0);
            }
        }

        // Each slot accumulates its ONE voice via the shared
        // `VoiceSlot::process_into` (the same per-variant `VoiceSource` match a
        // standalone `VoiceNode` uses).
        for slot in &mut self.voices {
            slot.process_into(size, n, output);
        }
    }

    audio_unit_boilerplate!(id = VOICE_POOL_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // Width must track `outputs()` or fundsp mis-plans this node's latency.
        SignalFrame::new(self.channels)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>() + self.voices.len() * std::mem::size_of::<VoiceSlot>()
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
