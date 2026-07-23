//! Per-track clip reader: a single graph node that internally manages
//! all audio clip playback for one track.
//!
//! Replaces the old "one `SamplerUnit` graph node per clip + dynamic
//! `StereoSumUnit`" model. ECS systems send [`ClipCommand`]s through
//! a [`TrackClipReaderHandle`]; the unit drains them each audio buffer.
//!
//! Each clip is EITHER an in-memory [`SamplerUnit`] (the whole clip resident in
//! RAM as an `Arc<Wave>`, decoded once by the wave cache) OR a
//! [`StreamingClipReader`] that pulls incrementally from the butler ring
//! (disk streaming). The choice is a monomorphized [`ClipSource`] enum,
//! not a boxed trait object, so the per-buffer match stays inlinable and the hot
//! path allocation-free. The optional time-stretch processor wraps whichever
//! source when a clip is stretched/pitched (both variants `impl AudioUnit`).

use std::sync::Arc;

#[cfg(feature = "bevy")]
use bevy_ecs::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use tutti_core::{
    AudioUnit, Beat, BeatDuration, BufferMut, BufferRef, Cents, Linear, Ratio, SamplePosition,
    SignalFrame, Timeline, Wave,
};

use crate::stretch;
use crate::ClipReader;
use crate::Command;
use crate::Commands;
use crate::LoopSetting;
use crate::SamplerUnit;
use crate::StreamingClipReader;

const COMMAND_CAPACITY: usize = 64;
const TRACK_CLIP_READER_ID: u64 = 0x_0000_0000_0000_DA03;

// ---------------------------------------------------------------------------
// Slot ID — opaque u128 so bevy-tutti stays independent of dawai-types.
// dawai-model converts ClipId ↔ SlotId at the boundary.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(pub u128);

// ---------------------------------------------------------------------------
// Direction — playback direction for a clip. Replaces loose `reverse: bool`
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
// ClipSource — a clip's audio source, monomorphized. Either the whole clip is
// resident in RAM (`InMemory`) or it streams incrementally from the butler ring
// (`Streaming`).
//
// DESIGN INVARIANT: the two variants differ ONLY in the *essential* per-sample
// read — `InMemory` indexes an `Arc<Wave>`; `Streaming` pops the butler-fed ring,
// emitting silence while `is_seeking()` and crossfading on refill. Every *cold*
// control op (gain, placement, loop, wave swap, seek, play/stop, reset,
// set_sample_rate) is unified through the `ClipReader` trait — see
// `as_clip_reader_mut` — so the command drain no longer branches per backend.
//
// The enum (not a `Box<dyn AudioUnit>`) is deliberate: RT requires monomorphized
// dispatch on `tick`/`process`, so the per-sample read inlines and never touches
// a vtable or the heap. `ClipReader` is a trait object only on the cold path.
// Both variants are `Clone` and `impl AudioUnit`, so the field-wise `ClipSlot`
// clone and the stretch wrapper work uniformly across them.
// ---------------------------------------------------------------------------

enum ClipSource {
    InMemory(SamplerUnit),
    Streaming(StreamingClipReader),
}

impl Clone for ClipSource {
    fn clone(&self) -> Self {
        match self {
            Self::InMemory(s) => Self::InMemory(s.clone()),
            Self::Streaming(s) => Self::Streaming(s.clone()),
        }
    }
}

impl ClipSource {
    /// The direct-read source as `&mut dyn ClipReader` — the single cold-path
    /// control surface. Collapses the former `InMemory`/`Streaming`-specific
    /// mutation into one `ClipReader` call, so the command drain no longer
    /// branches per backend.
    #[inline]
    fn as_clip_reader_mut(&mut self) -> &mut dyn ClipReader {
        match self {
            Self::InMemory(s) => s,
            Self::Streaming(s) => s,
        }
    }
}

// ---------------------------------------------------------------------------
// ClipSlot — one clip's playback state. Its `source` is either an in-memory
// `SamplerUnit` or a streaming `StreamingClipReader` (the [`ClipSource`] enum).
// `id` / `direction` / stretch state are shared across both variants.
// ---------------------------------------------------------------------------

struct ClipSlot {
    id: SlotId,
    source: ClipSource,
    direction: Direction,
    /// Butler channel index for a `Streaming` source; `None` for `InMemory`.
    /// The reader drain forwards streaming loop ops (`SetStreamLoop` /
    /// `ClearStreamLoop`) to this channel via the typed [`Commands`] handle —
    /// loop is butler-owned (it reads a fadein head off disk + mutates
    /// `plan.link.loop_config`, neither reachable from the reader), so the
    /// forward is the honest path. Meaningless for `InMemory` (loop is primed
    /// directly on the `SamplerUnit`).
    channel_index: Option<usize>,
    /// The time-stretch processor is **always resident**: it is built once (two
    /// phase-vocoder constructions + four `RtScratch` scratch buffers) when the
    /// slot is created, off the per-buffer hot path. The audio thread never
    /// (re)builds it — it only flips the lock-free `stretch_factor` /
    /// `pitch_cents` atomics inside it. It owns NO copy of the clip source: it is
    /// a pure frame-in → frame-out filter. At tick time, the `needs_stretch()`
    /// gate (mirrored from those atomics into the two factor fields below)
    /// chooses whether to tick the single `source` and route its frame through
    /// this filter, or read `source` directly. The heavy construction stays off
    /// the audio thread this way: [`ClipCommand::UpdateStretch`] only sets
    /// atomics, never allocates.
    ///
    /// Structural invariant: the factor fields cannot
    /// drift from the processor's atomics — every mutation goes through
    /// [`ClipSlot::set_stretch`], which writes both in one step, and the fields
    /// are private so no caller can set a non-identity factor without the
    /// matching atomic being updated.
    stretch: stretch::Unit,
    stretch_factor: Ratio,
    pitch_cents: Cents,
    sample_rate: f64,
}

impl ClipSlot {
    /// Build a slot with the resident stretch unit already materialised and its
    /// atomics primed from `stretch_factor` / `pitch_cents` — the single
    /// constructor both the live `Add` path and the synchronous `insert_clip`
    /// path go through. The heavy `stretch::Unit` construction happens here, at
    /// slot-creation time, never on the per-buffer command drain.
    fn new(
        id: SlotId,
        source: ClipSource,
        direction: Direction,
        channel_index: Option<usize>,
        stretch_factor: Ratio,
        pitch_cents: Cents,
        sample_rate: f64,
    ) -> Self {
        let stretch = stretch::Unit::new(sample_rate);
        stretch.set_stretch_factor(stretch_factor);
        stretch.set_pitch_cents(pitch_cents);
        Self {
            id,
            source,
            direction,
            channel_index,
            stretch,
            stretch_factor,
            pitch_cents,
            sample_rate,
        }
    }

    fn needs_stretch(&self) -> bool {
        (self.stretch_factor.get() - 1.0).abs() > 0.001 || self.pitch_cents.get().abs() > 0.5
    }

    /// Update the stretch factors — the only entry point for mutating them.
    /// Lock-free: flips the resident processor's atomics and mirrors the values
    /// into the factor fields (used by the `needs_stretch()` routing gate).
    /// Allocation-free, so it is safe to run on the audio-thread command drain.
    fn set_stretch(&mut self, stretch_factor: Ratio, pitch_cents: Cents) {
        self.stretch_factor = stretch_factor;
        self.pitch_cents = pitch_cents;
        self.stretch.set_stretch_factor(stretch_factor);
        self.stretch.set_pitch_cents(pitch_cents);
    }
}

// ---------------------------------------------------------------------------
// Commands sent from ECS → audio thread.
// ---------------------------------------------------------------------------

pub enum ClipCommand {
    Add {
        id: SlotId,
        sampler: SamplerUnit,
        direction: Direction,
    },
    /// Add a disk-streaming clip. The `StreamingClipReader` is built on the
    /// ECS/butler side (butler stream registration, ring allocation, placement
    /// gate), so the audio-thread drain only moves it into a slot — no
    /// allocation or I/O on the hot path.
    AddStreaming {
        id: SlotId,
        reader: StreamingClipReader,
        direction: Direction,
        /// Butler channel index the stream occupies. Stored on the slot so the
        /// reader drain can forward streaming loop ops to the right channel.
        channel_index: usize,
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
        gain: Linear,
    },
    UpdateSpeed {
        id: SlotId,
        speed: Ratio,
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
        stretch_factor: Ratio,
        pitch_cents: Cents,
    },
}

// ---------------------------------------------------------------------------
// ClipSpec — a fully-described in-memory clip for synchronous insertion.
// ---------------------------------------------------------------------------

/// One in-memory clip, ready to drop into a reader's slot list without going
/// through the command channel. Used by the offline region render, which
/// populates a cloned (never-ticked) reader directly from ECS state.
///
/// Carries the same surface as `ClipCommand::Add` plus stretch, so a single
/// insert reproduces what the live path builds across `Add` + `UpdateStretch`.
pub struct ClipSpec {
    pub id: SlotId,
    /// Already transport-bound, with gain / loop range applied.
    pub sampler: SamplerUnit,
    pub direction: Direction,
    pub stretch_factor: Ratio,
    pub pitch_cents: Cents,
}

// ---------------------------------------------------------------------------
// Handle — held by ECS systems, sends commands to the audio-thread unit.
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct TrackClipReaderHandle {
    tx: Sender<ClipCommand>,
}

impl TrackClipReaderHandle {
    pub fn send(&self, cmd: ClipCommand) {
        match self.tx.try_send(cmd) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                #[cfg(feature = "bevy")]
                bevy_log::warn!("TrackClipReader command queue full, dropping command");
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// ECS components — live on the track entity.
// ---------------------------------------------------------------------------

#[cfg(feature = "bevy")]
#[derive(Component)]
pub struct TrackClipReaderRef(pub TrackClipReaderHandle);

#[cfg(feature = "bevy")]
#[derive(Component, Debug, Clone, Copy)]
pub struct TrackClipReaderNode(pub tutti_core::NodeId);

// ---------------------------------------------------------------------------
// TrackClipReaderUnit — the AudioUnit.
// ---------------------------------------------------------------------------

pub struct TrackClipReaderUnit {
    clips: Vec<ClipSlot>,
    rx: Receiver<ClipCommand>,
    sample_rate: f64,
    transport: Option<Arc<dyn Timeline>>,
    /// Typed butler write handle. `Some` on the live path (threaded in from the
    /// [`Sampler`](crate::Sampler)); `None` for tests / detached / offline
    /// readers with no live butler. Used by the drain to forward *streaming*
    /// loop ops (`Command::Loop`) — loop is butler-owned and not reachable from
    /// the reader itself. Cloning it is cheap (a `Sender` + an `Arc` map).
    butler: Option<Commands>,
}

impl TrackClipReaderUnit {
    /// Build a unit from an already-created command receiver + optional
    /// transport. Shared field-literal source for `new` / `with_transport` /
    /// `detached`.
    fn from_parts(
        rx: Receiver<ClipCommand>,
        transport: Option<Arc<dyn Timeline>>,
        butler: Option<Commands>,
    ) -> Self {
        Self {
            clips: Vec::new(),
            rx,
            sample_rate: 44100.0,
            transport,
            butler,
        }
    }

    pub fn new() -> (Self, TrackClipReaderHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let handle = TrackClipReaderHandle { tx };
        (Self::from_parts(rx, None, None), handle)
    }

    pub fn with_transport(
        transport: Arc<dyn Timeline>,
        butler: Option<Commands>,
    ) -> (Self, TrackClipReaderHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let handle = TrackClipReaderHandle { tx };
        (Self::from_parts(rx, Some(transport), butler), handle)
    }

    /// Number of clip slots currently materialised (drained from the command
    /// queue). Diagnostic / test helper.
    pub fn clip_count(&self) -> usize {
        self.clips.len()
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
    /// carry live clip state. Rather than clone-then-sever, the render Prepare
    /// step *replaces* each reader node with one of these: born empty, born
    /// channel-less, so it shares zero mutable state with the live graph at any
    /// instant. Clips are then rebuilt from ECS in the Populate step via
    /// [`Self::insert_clip`].
    pub fn detached(transport: Arc<dyn Timeline>) -> Self {
        let (_tx, rx) = bounded(0);
        // No butler: the offline render never forwards streaming loop ops (it
        // rebuilds in-RAM clips from ECS), so a `None` handle is correct here.
        Self::from_parts(rx, Some(transport), None)
    }

    /// Insert a clip slot directly, bypassing the command channel.
    ///
    /// The live path adds clips by sending `ClipCommand::Add` and letting the
    /// audio thread drain it in `tick`/`process`. A cloned net built for the
    /// offline render is never ticked on a thread that drains, so it needs its
    /// clips materialised synchronously — that's this. Same replace-by-id then
    /// push semantics as the `ClipCommand::Add` drain arm, plus stretch.
    pub fn insert_clip(&mut self, spec: ClipSpec) {
        self.clips.retain(|s| s.id != spec.id);
        self.clips.push(ClipSlot::new(
            spec.id,
            ClipSource::InMemory(spec.sampler),
            spec.direction,
            None,
            spec.stretch_factor,
            spec.pitch_cents,
            self.sample_rate,
        ));
    }

    /// Drop every clip slot.
    ///
    /// The offline render clones the staged net and then rebuilds each clip
    /// fresh from ECS + the wave cache; clearing the inherited slots first
    /// keeps the cloned reader from carrying any state tied to the live graph.
    pub fn clear_clips(&mut self) {
        self.clips.clear();
    }

    fn slot_mut(&mut self, id: SlotId) -> Option<&mut ClipSlot> {
        self.clips.iter_mut().find(|s| s.id == id)
    }

    /// Apply a loop setting to a slot, routing by tier.
    ///
    /// - **In-RAM**: primes / clears the loop range on the `SamplerUnit` in-unit
    ///   (`ClipReader::set_loop`).
    /// - **Streaming**: loop is butler-owned — `SetStreamLoop` reads a loop-start
    ///   fadein head off disk and mutates `plan.link.loop_config`, neither
    ///   reachable from the reader — so the reader FORWARDS to the butler via the
    ///   typed [`Commands`] handle (`Command::Loop`, which maps `On`→
    ///   `SetStreamLoop` / `Off`→`ClearStreamLoop`). This is the same command
    ///   dawai-model used to send itself; it now originates here so dawai speaks
    ///   one unified `ClipCommand` for both tiers.
    ///
    /// RT-safe: this runs on the COLD command drain (top of `tick`/`process`,
    /// before the per-sample loop), so the channel send is fine — it never
    /// touches the per-sample hot path.
    fn apply_loop(&mut self, id: SlotId, setting: LoopSetting) {
        let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) else {
            return;
        };
        match &mut slot.source {
            ClipSource::InMemory(sampler) => {
                sampler.set_loop(setting);
            }
            ClipSource::Streaming(_) => {
                if let (Some(butler), Some(channel_index)) = (&self.butler, slot.channel_index) {
                    butler.send(Command::Loop {
                        channel_index,
                        setting,
                    });
                }
            }
        }
    }

    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                ClipCommand::Add {
                    id,
                    sampler,
                    direction,
                } => {
                    self.clips.retain(|s| s.id != id);
                    self.clips.push(ClipSlot::new(
                        id,
                        ClipSource::InMemory(sampler),
                        direction,
                        None,
                        Ratio::new(1.0),
                        Cents::new(0.0),
                        self.sample_rate,
                    ));
                }
                ClipCommand::AddStreaming {
                    id,
                    reader,
                    direction,
                    channel_index,
                } => {
                    // The reader is fully built on the ECS/butler side; the
                    // drain only moves it into a slot. `ClipSlot::new` builds the
                    // resident stretch unit (a `StreamingClipReader` clone) — the
                    // one heavy step — which is why the `AddStreaming` command,
                    // like `Add`, is a control-thread emission, not a hot-path op.
                    self.clips.retain(|s| s.id != id);
                    self.clips.push(ClipSlot::new(
                        id,
                        ClipSource::Streaming(reader),
                        direction,
                        Some(channel_index),
                        Ratio::new(1.0),
                        Cents::new(0.0),
                        self.sample_rate,
                    ));
                }
                ClipCommand::Remove(id) => {
                    self.clips.retain(|s| s.id != id);
                }
                ClipCommand::ReplaceWave { id, wave } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // In-memory: swap the resident wave. Streaming has no
                        // in-RAM wave; its `ClipReader::set_wave` is a deliberate
                        // no-op (a source change means re-registering the butler
                        // stream on a different file — a control-thread / butler
                        // op, `AddStreaming` re-issued control-side from
                        // dawai-model). One `ClipReader` call covers both.
                        slot.source.as_clip_reader_mut().set_wave(wave);
                    }
                }
                ClipCommand::UpdatePlacement {
                    id,
                    start_beat,
                    duration_beats,
                } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // Both backends carry a placement gate; each backend's
                        // `set_placement` re-arms its own (streaming re-seeks on
                        // the next inside-frame).
                        slot.source
                            .as_clip_reader_mut()
                            .set_placement(start_beat, duration_beats);
                    }
                }
                ClipCommand::UpdateGain { id, gain } => {
                    if let Some(slot) = self.slot_mut(id) {
                        slot.source.as_clip_reader_mut().set_gain(gain);
                    }
                }
                ClipCommand::UpdateSpeed { id, speed } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // Unified across tiers: both backends carry speed in-unit.
                        // In-RAM stores it on the `SamplerUnit`; streaming forwards
                        // to the shared `RtState` (the exact speed effect of the
                        // butler's `SetVarispeed`, reachable from the reader). The
                        // `ClipReader::set_speed` covers both — dawai no longer
                        // forks streaming speed onto a separate butler command.
                        slot.source.as_clip_reader_mut().set_speed(speed);
                    }
                }
                ClipCommand::UpdateLoop {
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
                ClipCommand::ClearLoop(id) => {
                    self.apply_loop(id, LoopSetting::Off);
                }
                ClipCommand::UpdateReverse { id, direction } => {
                    if let Some(slot) = self.slot_mut(id) {
                        // In-RAM: `slot.direction` drives the reversed index in the
                        // hot read (the source-side `set_direction` is a no-op).
                        // Streaming: the source-side `set_direction` forwards to the
                        // shared `RtState` (the direction leg of the butler's
                        // `SetVarispeed`) — `slot.direction` is unused by the ring
                        // pull. One command reaches both, so dawai sends reverse
                        // ONCE, no longer folding it into a separate butler speed
                        // command.
                        slot.direction = direction;
                        slot.source.as_clip_reader_mut().set_direction(direction);
                    }
                }
                ClipCommand::UpdateStretch {
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

    #[inline]
    fn read_clip_sample(sampler: &SamplerUnit, direction: Direction, pos: f64) -> (f32, f32) {
        match direction {
            Direction::Reverse => {
                let len = sampler.duration_samples() as f64;
                let reversed = (len - 1.0 - pos).max(0.0);
                sampler.get_sample(reversed)
            }
            Direction::Forward => sampler.get_sample(pos),
        }
    }

    /// Read ONE raw stereo frame from the single clip source, using the exact
    /// per-variant read the direct (non-stretch) `tick` else-branch uses — the
    /// `ClipSource` enum still owns the read. Alloc-free: returns a stack frame.
    /// Used to feed the stretch filter (which owns no source) on the `tick` hot
    /// path.
    #[inline]
    fn read_source_frame(source: &mut ClipSource, direction: Direction) -> [f32; 2] {
        match source {
            ClipSource::InMemory(sampler) => match sampler.transport_sample_position() {
                Some(pos) => {
                    let (l, r) = Self::read_clip_sample(sampler, direction, pos);
                    [l, r]
                }
                None => [0.0, 0.0],
            },
            ClipSource::Streaming(reader) => {
                let mut buf = [0.0f32; 2];
                reader.tick(&[], &mut buf);
                buf
            }
        }
    }
}

impl Clone for TrackClipReaderUnit {
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
            clips: self
                .clips
                .iter()
                .map(|s| ClipSlot {
                    id: s.id,
                    source: s.source.clone(),
                    direction: s.direction,
                    channel_index: s.channel_index,
                    stretch: s.stretch.clone(),
                    stretch_factor: s.stretch_factor,
                    pitch_cents: s.pitch_cents,
                    sample_rate: s.sample_rate,
                }) // ClipSource + resident stretch::Unit clone by value; atomics preserved
                .collect(),
            rx: self.rx.clone(),
            sample_rate: self.sample_rate,
            transport: self.transport.clone(),
            butler: self.butler.clone(),
        }
    }
}

impl AudioUnit for TrackClipReaderUnit {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        for slot in &mut self.clips {
            slot.source.as_clip_reader_mut().reset();
            slot.stretch.reset();
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate.get();
        for slot in &mut self.clips {
            slot.source.as_clip_reader_mut().set_sample_rate(sample_rate);
            slot.sample_rate = sample_rate.get();
            slot.stretch.set_sample_rate(sample_rate);
        }
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        self.drain_commands();

        if output.len() < 2 {
            return;
        }

        let mut left = 0.0_f32;
        let mut right = 0.0_f32;

        for slot in &mut self.clips {
            if slot.needs_stretch() {
                // Tick the SINGLE source once to get its raw frame (the same
                // per-variant read the else-branch uses — the `ClipSource` enum
                // still owns the read), then feed that frame into the stretch
                // filter. Alloc-free: stack `[f32; 2]`, no heap.
                let raw = Self::read_source_frame(&mut slot.source, slot.direction);
                let mut buf = [0.0f32; 2];
                slot.stretch.tick(&raw, &mut buf);
                left += buf[0];
                right += buf[1];
            } else {
                match &mut slot.source {
                    ClipSource::InMemory(sampler) => {
                        if let Some(pos) = sampler.transport_sample_position() {
                            let (l, r) = Self::read_clip_sample(sampler, slot.direction, pos);
                            left += l;
                            right += r;
                        }
                    }
                    ClipSource::Streaming(reader) => {
                        // The `StreamingClipReader` owns its placement gate: it
                        // emits silence outside the clip window and pulls the
                        // butler ring inside it. Alloc-free (preallocated
                        // `fetch_scratch`).
                        let mut buf = [0.0f32; 2];
                        reader.tick(&[], &mut buf);
                        left += buf[0];
                        right += buf[1];
                    }
                }
            }
        }

        output[0] = left;
        output[1] = right;
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        self.drain_commands();

        for i in 0..size {
            output.set_f32(0, i, 0.0);
            output.set_f32(1, i, 0.0);
        }

        for slot in &mut self.clips {
            if slot.needs_stretch() {
                // Per-sample: read the SINGLE source frame (same per-variant read
                // as the else-branch — the enum still owns the read), then feed
                // it through the stretch filter. Alloc-free: stack `[f32; 2]`.
                match &mut slot.source {
                    ClipSource::InMemory(sampler) => {
                        let Some(start_pos) = sampler.transport_sample_position() else {
                            continue;
                        };
                        let advance = (sampler.speed().get() * sampler.src_ratio().get()) as f64;
                        let mut tick_out = [0.0f32; 2];
                        for i in 0..size {
                            let pos = start_pos + i as f64 * advance;
                            let (l, r) = Self::read_clip_sample(sampler, slot.direction, pos);
                            slot.stretch.tick(&[l, r], &mut tick_out);
                            output.set_f32(0, i, output.at_f32(0, i) + tick_out[0]);
                            output.set_f32(1, i, output.at_f32(1, i) + tick_out[1]);
                        }
                    }
                    ClipSource::Streaming(reader) => {
                        let mut raw = [0.0f32; 2];
                        let mut tick_out = [0.0f32; 2];
                        for i in 0..size {
                            reader.tick(&[], &mut raw);
                            slot.stretch.tick(&raw, &mut tick_out);
                            output.set_f32(0, i, output.at_f32(0, i) + tick_out[0]);
                            output.set_f32(1, i, output.at_f32(1, i) + tick_out[1]);
                        }
                    }
                }
            } else {
                match &mut slot.source {
                    ClipSource::InMemory(sampler) => {
                        let Some(start_pos) = sampler.transport_sample_position() else {
                            continue;
                        };
                        let advance = (sampler.speed().get() * sampler.src_ratio().get()) as f64;
                        for i in 0..size {
                            let pos = start_pos + i as f64 * advance;
                            let (l, r) = Self::read_clip_sample(sampler, slot.direction, pos);
                            output.set_f32(0, i, output.at_f32(0, i) + l);
                            output.set_f32(1, i, output.at_f32(1, i) + r);
                        }
                    }
                    ClipSource::Streaming(reader) => {
                        // Sum the reader per-sample (its placement gate + ring
                        // pull run inside each `tick`). Per-sample accumulation
                        // mirrors the stretch branch above and keeps this
                        // alloc-free — no per-slot scratch `BufferMut`.
                        let mut tick_out = [0.0f32; 2];
                        for i in 0..size {
                            reader.tick(&[], &mut tick_out);
                            output.set_f32(0, i, output.at_f32(0, i) + tick_out[0]);
                            output.set_f32(1, i, output.at_f32(1, i) + tick_out[1]);
                        }
                    }
                }
            }
        }
    }

    audio_unit_boilerplate!(id = TRACK_CLIP_READER_ID);

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(2)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>() + self.clips.len() * std::mem::size_of::<ClipSlot>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct MockTransport {
        playing: AtomicBool,
        beat: AtomicU64,
        tempo: AtomicU64,
    }

    impl MockTransport {
        fn new(tempo: f64, beat: f64, playing: bool) -> Arc<Self> {
            Arc::new(Self {
                playing: AtomicBool::new(playing),
                beat: AtomicU64::new(beat.to_bits()),
                tempo: AtomicU64::new(tempo.to_bits()),
            })
        }
    }

    impl Timeline for MockTransport {
        fn is_rolling(&self) -> bool {
            self.playing.load(Ordering::Relaxed)
        }
        fn beat(&self) -> tutti_core::Beat {
            tutti_core::Beat(f64::from_bits(self.beat.load(Ordering::Relaxed)))
        }
        fn tempo(&self) -> tutti_core::Bpm {
            tutti_core::Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
        }
        fn loop_range(&self) -> Option<tutti_core::LoopRange> {
            None
        }
    }

    fn make_wave(samples: usize) -> Arc<Wave> {
        let data: Vec<f32> = (0..samples)
            .map(|i| (i as f32 + 1.0) / samples as f32)
            .collect();
        Arc::new(Wave::from_samples(44100.0, &data))
    }

    #[test]
    fn add_and_remove_clips() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        let sampler = SamplerUnit::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
        handle.send(ClipCommand::Add {
            id: SlotId(1),
            sampler,
            direction: Direction::Forward,
        });

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(out[0] != 0.0 || out[1] != 0.0, "clip should produce audio");

        handle.send(ClipCommand::Remove(SlotId(1)));
        unit.tick(&[], &mut out);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn silence_when_transport_stopped() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, false);
        let wave = make_wave(100);

        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        handle.send(ClipCommand::Add {
            id: SlotId(1),
            sampler,
            direction: Direction::Forward,
        });

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);
    }

    #[test]
    fn clips_sum_together() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        for i in 0..3 {
            let sampler =
                SamplerUnit::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
            handle.send(ClipCommand::Add {
                id: SlotId(i),
                sampler,
                direction: Direction::Forward,
            });
        }

        let mut out_3 = [0.0f32; 2];
        unit.tick(&[], &mut out_3);

        let (mut unit2, handle2) = TrackClipReaderUnit::new();
        let sampler = SamplerUnit::with_transport(wave.clone(), transport.clone(), Beat::new(0.0), None);
        handle2.send(ClipCommand::Add {
            id: SlotId(0),
            sampler,
            direction: Direction::Forward,
        });
        let mut out_1 = [0.0f32; 2];
        unit2.tick(&[], &mut out_1);

        let tolerance = 0.001;
        assert!((out_3[0] - out_1[0] * 3.0).abs() < tolerance);
        assert!((out_3[1] - out_1[1] * 3.0).abs() < tolerance);
    }

    #[test]
    fn clone_snapshots_clips() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        handle.send(ClipCommand::Add {
            id: SlotId(1),
            sampler,
            direction: Direction::Forward,
        });

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);

        let mut cloned = unit.clone();
        let mut out_clone = [0.0f32; 2];
        cloned.tick(&[], &mut out_clone);

        assert!(out_clone[0] != 0.0, "cloned unit should have the clip");
    }

    #[test]
    fn insert_clip_is_audible_without_channel() {
        let (mut unit, _handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        unit.insert_clip(ClipSpec {
            id: SlotId(1),
            sampler,
            direction: Direction::Forward,
            stretch_factor: Ratio::new(1.0),
            pitch_cents: Cents::new(0.0),
        });

        // No tick/drain needed — the clip is already in the slot list.
        assert_eq!(unit.clip_count(), 1);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(
            out[0] != 0.0 || out[1] != 0.0,
            "inserted clip should produce audio"
        );
    }

    #[test]
    fn detached_reader_has_no_channel_and_is_empty() {
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        // A render-only reader is born empty and channel-less: there is no
        // sender for its `rx`, so no command can ever reach it.
        let mut unit = TrackClipReaderUnit::detached(transport.clone());
        assert_eq!(unit.clip_count(), 0, "detached reader starts empty");

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out); // drains its (permanently empty) queue
        assert_eq!(unit.clip_count(), 0, "detached reader receives no commands");
        assert_eq!(out[0], 0.0);
        assert_eq!(out[1], 0.0);

        // But clips inserted directly (the Populate path) are audible.
        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        unit.insert_clip(ClipSpec {
            id: SlotId(1),
            sampler,
            direction: Direction::Forward,
            stretch_factor: Ratio::new(1.0),
            pitch_cents: Cents::new(0.0),
        });
        unit.tick(&[], &mut out);
        assert!(out[0] != 0.0 || out[1] != 0.0, "inserted clip is audible");
    }

    #[test]
    fn update_gain() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        handle.send(ClipCommand::Add {
            id: SlotId(1),
            sampler,
            direction: Direction::Forward,
        });

        let mut out_before = [0.0f32; 2];
        unit.tick(&[], &mut out_before);

        handle.send(ClipCommand::UpdateGain {
            id: SlotId(1),
            gain: Linear::new(0.5),
        });

        let mut out_after = [0.0f32; 2];
        unit.tick(&[], &mut out_after);

        assert!((out_after[0] - out_before[0] * 0.5).abs() < 0.01);
    }

    #[test]
    fn update_stretch_enables_processor() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(4096);

        let sampler = SamplerUnit::with_transport(wave, transport, Beat::new(0.0), None);
        handle.send(ClipCommand::Add {
            id: SlotId(1),
            sampler,
            direction: Direction::Forward,
        });

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(!unit.clips[0].needs_stretch(), "no stretch by default");

        handle.send(ClipCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: Ratio::new(2.0),
            pitch_cents: Cents::new(0.0),
        });
        unit.tick(&[], &mut out);
        assert!(unit.clips[0].needs_stretch(), "stretch should be active");

        handle.send(ClipCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: Ratio::new(1.0),
            pitch_cents: Cents::new(0.0),
        });
        unit.tick(&[], &mut out);
        assert!(
            !unit.clips[0].needs_stretch(),
            "identity stretch disables processor"
        );
    }
}
