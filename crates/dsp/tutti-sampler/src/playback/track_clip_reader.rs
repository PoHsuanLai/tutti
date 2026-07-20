//! Per-track clip reader: a single graph node that internally manages
//! all audio clip playback for one track.
//!
//! Replaces the old "one `SamplerUnit` graph node per clip + dynamic
//! `StereoSumUnit`" model. ECS systems send [`ClipCommand`]s through
//! a [`TrackClipReaderHandle`]; the unit drains them each audio buffer.
//!
//! Every clip is an in-memory [`SamplerUnit`] — the whole clip resides in RAM
//! as an `Arc<Wave>` (decoded once by the wave cache). The optional time-stretch
//! processor wraps the sampler when a clip is stretched/pitched.

use std::sync::Arc;

#[cfg(feature = "bevy")]
use bevy_ecs::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame, TransportReader, Wave};
use crate::stretch;
use crate::SamplerUnit;

const COMMAND_CAPACITY: usize = 64;
const TRACK_CLIP_READER_ID: u64 = 0x_0000_0000_0000_DA03;

// ---------------------------------------------------------------------------
// Slot ID — opaque u128 so bevy-tutti stays independent of dawai-types.
// dawai-model converts ClipId ↔ SlotId at the boundary.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(pub u128);

// ---------------------------------------------------------------------------
// ClipSlot — one clip's playback state. Always an in-memory `SamplerUnit`
// (the whole clip resides in RAM as an `Arc<Wave>`, decoded once by the wave
// cache). Disk streaming was removed: it didn't actually stream (it decoded
// the whole file then served from RAM), and its single-consumer ring couldn't
// be cloned safely for the offline render.
// ---------------------------------------------------------------------------

struct ClipSlot {
    id: SlotId,
    sampler: SamplerUnit,
    reverse: bool,
    stretch: Option<stretch::Unit>,
    stretch_factor: f32,
    pitch_cents: f32,
    sample_rate: f64,
}

impl ClipSlot {
    fn needs_stretch(&self) -> bool {
        (self.stretch_factor - 1.0).abs() > 0.001 || self.pitch_cents.abs() > 0.5
    }

    fn rebuild_stretch(&mut self) {
        if self.needs_stretch() {
            let unit = stretch::Unit::new(Box::new(self.sampler.clone()), self.sample_rate);
            unit.set_stretch_factor(self.stretch_factor);
            unit.set_pitch_cents(self.pitch_cents);
            self.stretch = Some(unit);
        } else {
            self.stretch = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Commands sent from ECS → audio thread.
// ---------------------------------------------------------------------------

pub enum ClipCommand {
    Add {
        id: SlotId,
        sampler: SamplerUnit,
        reverse: bool,
    },
    Remove(SlotId),
    ReplaceWave {
        id: SlotId,
        wave: Arc<Wave>,
    },
    UpdatePlacement {
        id: SlotId,
        start_beat: f64,
        duration_beats: Option<f64>,
    },
    UpdateGain {
        id: SlotId,
        gain: f32,
    },
    UpdateSpeed {
        id: SlotId,
        speed: f32,
    },
    UpdateLoop {
        id: SlotId,
        looping: bool,
        loop_start: u64,
        loop_end: u64,
        crossfade_samples: usize,
    },
    ClearLoop(SlotId),
    UpdateReverse {
        id: SlotId,
        reverse: bool,
    },
    UpdateStretch {
        id: SlotId,
        stretch_factor: f32,
        pitch_cents: f32,
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
    pub reverse: bool,
    pub stretch_factor: f32,
    pub pitch_cents: f32,
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
    transport: Option<Arc<dyn TransportReader>>,
}

impl TrackClipReaderUnit {
    pub fn new() -> (Self, TrackClipReaderHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let handle = TrackClipReaderHandle { tx };
        let unit = Self {
            clips: Vec::new(),
            rx,
            sample_rate: 44100.0,
            transport: None,
        };
        (unit, handle)
    }

    pub fn with_transport(
        transport: Arc<dyn TransportReader>,
    ) -> (Self, TrackClipReaderHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let handle = TrackClipReaderHandle { tx };
        let unit = Self {
            clips: Vec::new(),
            rx,
            sample_rate: 44100.0,
            transport: Some(transport),
        };
        (unit, handle)
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
    pub fn detached(transport: Arc<dyn TransportReader>) -> Self {
        let (_tx, rx) = bounded(0);
        Self {
            clips: Vec::new(),
            rx,
            sample_rate: 44100.0,
            transport: Some(transport),
        }
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
        let mut slot = ClipSlot {
            id: spec.id,
            sampler: spec.sampler,
            reverse: spec.reverse,
            stretch: None,
            stretch_factor: spec.stretch_factor,
            pitch_cents: spec.pitch_cents,
            sample_rate: self.sample_rate,
        };
        slot.rebuild_stretch();
        self.clips.push(slot);
    }

    /// Drop every clip slot.
    ///
    /// The offline render clones the staged net and then rebuilds each clip
    /// fresh from ECS + the wave cache; clearing the inherited slots first
    /// keeps the cloned reader from carrying any state tied to the live graph.
    pub fn clear_clips(&mut self) {
        self.clips.clear();
    }

    fn drain_commands(&mut self) {
        while let Ok(cmd) = self.rx.try_recv() {
            match cmd {
                ClipCommand::Add {
                    id,
                    sampler,
                    reverse,
                } => {
                    self.clips.retain(|s| s.id != id);
                    self.clips.push(ClipSlot {
                        id,
                        sampler,
                        reverse,
                        stretch: None,
                        stretch_factor: 1.0,
                        pitch_cents: 0.0,
                        sample_rate: self.sample_rate,
                    });
                }
                ClipCommand::Remove(id) => {
                    self.clips.retain(|s| s.id != id);
                }
                ClipCommand::ReplaceWave { id, wave } => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        slot.sampler.set_wave(wave);
                        slot.rebuild_stretch();
                    }
                }
                ClipCommand::UpdatePlacement {
                    id,
                    start_beat,
                    duration_beats,
                } => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        slot.sampler.set_placement(start_beat, duration_beats);
                    }
                }
                ClipCommand::UpdateGain { id, gain } => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        slot.sampler.set_gain(gain);
                    }
                }
                ClipCommand::UpdateSpeed { id, speed } => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        slot.sampler.set_speed(speed);
                    }
                }
                ClipCommand::UpdateLoop {
                    id,
                    looping,
                    loop_start,
                    loop_end,
                    crossfade_samples,
                } => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        if looping {
                            slot.sampler
                                .set_loop_range(loop_start, loop_end, crossfade_samples);
                        } else {
                            slot.sampler.clear_loop_range();
                            slot.sampler.set_looping(false);
                        }
                    }
                }
                ClipCommand::ClearLoop(id) => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        slot.sampler.clear_loop_range();
                        slot.sampler.set_looping(false);
                    }
                }
                ClipCommand::UpdateReverse { id, reverse } => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        slot.reverse = reverse;
                    }
                }
                ClipCommand::UpdateStretch {
                    id,
                    stretch_factor,
                    pitch_cents,
                } => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        slot.stretch_factor = stretch_factor;
                        slot.pitch_cents = pitch_cents;
                        slot.rebuild_stretch();
                    }
                }
            }
        }
    }

    #[inline]
    fn read_clip_sample(sampler: &SamplerUnit, reverse: bool, pos: f64) -> (f32, f32) {
        if reverse {
            let len = sampler.duration_samples() as f64;
            let reversed = (len - 1.0 - pos).max(0.0);
            sampler.get_sample(reversed)
        } else {
            sampler.get_sample(pos)
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
                    sampler: s.sampler.clone(),
                    reverse: s.reverse,
                    stretch: s.stretch.clone(),
                    stretch_factor: s.stretch_factor,
                    pitch_cents: s.pitch_cents,
                    sample_rate: s.sample_rate,
                })
                .collect(),
            rx: self.rx.clone(),
            sample_rate: self.sample_rate,
            transport: self.transport.clone(),
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
            slot.sampler.reset();
            if let Some(ref mut s) = slot.stretch {
                s.reset();
            }
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.sample_rate = sample_rate.get();
        for slot in &mut self.clips {
            slot.sampler.set_sample_rate(sample_rate);
            slot.sample_rate = sample_rate.get();
            if let Some(ref mut s) = slot.stretch {
                s.set_sample_rate(sample_rate);
            }
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
            if let Some(ref mut s) = slot.stretch {
                let mut buf = [0.0f32; 2];
                s.tick(&[], &mut buf);
                left += buf[0];
                right += buf[1];
            } else if let Some(pos) = slot.sampler.transport_sample_position() {
                let (l, r) = Self::read_clip_sample(&slot.sampler, slot.reverse, pos);
                left += l;
                right += r;
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
            if let Some(ref mut s) = slot.stretch {
                let mut tick_out = [0.0f32; 2];
                for i in 0..size {
                    s.tick(&[], &mut tick_out);
                    output.set_f32(0, i, output.at_f32(0, i) + tick_out[0]);
                    output.set_f32(1, i, output.at_f32(1, i) + tick_out[1]);
                }
            } else {
                let Some(start_pos) = slot.sampler.transport_sample_position() else {
                    continue;
                };
                let advance = (slot.sampler.speed() * slot.sampler.src_ratio()) as f64;
                for i in 0..size {
                    let pos = start_pos + i as f64 * advance;
                    let (l, r) = Self::read_clip_sample(&slot.sampler, slot.reverse, pos);
                    output.set_f32(0, i, output.at_f32(0, i) + l);
                    output.set_f32(1, i, output.at_f32(1, i) + r);
                }
            }
        }
    }

    fn get_id(&self) -> u64 {
        TRACK_CLIP_READER_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

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

    impl TransportReader for MockTransport {
        fn is_playing(&self) -> bool {
            self.playing.load(Ordering::Relaxed)
        }
        fn current_beat(&self) -> f64 {
            f64::from_bits(self.beat.load(Ordering::Relaxed))
        }
        fn tempo(&self) -> tutti_core::Bpm {
            tutti_core::Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
        }
        fn is_loop_enabled(&self) -> bool {
            false
        }
        fn get_loop_range(&self) -> Option<(f64, f64)> {
            None
        }
        fn is_recording(&self) -> bool {
            false
        }
        fn is_in_preroll(&self) -> bool {
            false
        }
    }

    fn make_wave(samples: usize) -> Arc<Wave> {
        let data: Vec<f32> = (0..samples).map(|i| (i as f32 + 1.0) / samples as f32).collect();
        Arc::new(Wave::from_samples(44100.0, &data))
    }

    #[test]
    fn add_and_remove_clips() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        let sampler = SamplerUnit::with_transport(wave.clone(), transport.clone(), 0.0, None);
        handle.send(ClipCommand::Add {
            id: SlotId(1),
            sampler,
            reverse: false,
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

        let sampler = SamplerUnit::with_transport(wave, transport, 0.0, None);
        handle.send(ClipCommand::Add {
            id: SlotId(1),
            sampler,
            reverse: false,
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
                SamplerUnit::with_transport(wave.clone(), transport.clone(), 0.0, None);
            handle.send(ClipCommand::Add {
                id: SlotId(i),
                sampler,
                reverse: false,
            });
        }

        let mut out_3 = [0.0f32; 2];
        unit.tick(&[], &mut out_3);

        let (mut unit2, handle2) = TrackClipReaderUnit::new();
        let sampler = SamplerUnit::with_transport(wave.clone(), transport.clone(), 0.0, None);
        handle2.send(ClipCommand::Add {
            id: SlotId(0),
            sampler,
            reverse: false,
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

        let sampler = SamplerUnit::with_transport(wave, transport, 0.0, None);
        handle.send(ClipCommand::Add {
            id: SlotId(1),
            sampler,
            reverse: false,
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

        let sampler = SamplerUnit::with_transport(wave, transport, 0.0, None);
        unit.insert_clip(ClipSpec {
            id: SlotId(1),
            sampler,
            reverse: false,
            stretch_factor: 1.0,
            pitch_cents: 0.0,
        });

        // No tick/drain needed — the clip is already in the slot list.
        assert_eq!(unit.clip_count(), 1);

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(out[0] != 0.0 || out[1] != 0.0, "inserted clip should produce audio");
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
        let sampler = SamplerUnit::with_transport(wave, transport, 0.0, None);
        unit.insert_clip(ClipSpec {
            id: SlotId(1),
            sampler,
            reverse: false,
            stretch_factor: 1.0,
            pitch_cents: 0.0,
        });
        unit.tick(&[], &mut out);
        assert!(out[0] != 0.0 || out[1] != 0.0, "inserted clip is audible");
    }

    #[test]
    fn update_gain() {
        let (mut unit, handle) = TrackClipReaderUnit::new();
        let transport = MockTransport::new(120.0, 0.0, true);
        let wave = make_wave(100);

        let sampler = SamplerUnit::with_transport(wave, transport, 0.0, None);
        handle.send(ClipCommand::Add {
            id: SlotId(1),
            sampler,
            reverse: false,
        });

        let mut out_before = [0.0f32; 2];
        unit.tick(&[], &mut out_before);

        handle.send(ClipCommand::UpdateGain {
            id: SlotId(1),
            gain: 0.5,
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

        let sampler = SamplerUnit::with_transport(wave, transport, 0.0, None);
        handle.send(ClipCommand::Add {
            id: SlotId(1),
            sampler,
            reverse: false,
        });

        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);
        assert!(unit.clips[0].stretch.is_none(), "no stretch by default");

        handle.send(ClipCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: 2.0,
            pitch_cents: 0.0,
        });
        unit.tick(&[], &mut out);
        assert!(unit.clips[0].stretch.is_some(), "stretch should be active");

        handle.send(ClipCommand::UpdateStretch {
            id: SlotId(1),
            stretch_factor: 1.0,
            pitch_cents: 0.0,
        });
        unit.tick(&[], &mut out);
        assert!(
            unit.clips[0].stretch.is_none(),
            "identity stretch disables processor"
        );
    }
}
