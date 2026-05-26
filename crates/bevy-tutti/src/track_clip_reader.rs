//! Per-track clip reader: a single graph node that internally manages
//! all audio clip playback for one track.
//!
//! Replaces the old "one `SamplerUnit` graph node per clip + dynamic
//! `StereoSumUnit`" model. ECS systems send [`ClipCommand`]s through
//! a [`TrackClipReaderHandle`]; the unit drains them each audio buffer.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use tutti::core::{AudioUnit, BufferMut, BufferRef, SignalFrame, Wave};
use tutti::sampler::SamplerUnit;

const COMMAND_CAPACITY: usize = 64;
const TRACK_CLIP_READER_ID: u64 = 0x_0000_0000_0000_DA03;

// ---------------------------------------------------------------------------
// Slot ID — opaque u128 so bevy-tutti stays independent of dawai-types.
// dawai-model converts ClipId ↔ SlotId at the boundary.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(pub u128);

// ---------------------------------------------------------------------------
// ClipSlot — one clip's playback state.
// ---------------------------------------------------------------------------

struct ClipSlot {
    id: SlotId,
    sampler: SamplerUnit,
    reverse: bool,
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
                bevy_log::warn!("TrackClipReader command queue full, dropping command");
            }
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

// ---------------------------------------------------------------------------
// ECS components — live on the track entity.
// ---------------------------------------------------------------------------

#[derive(Component)]
pub struct TrackClipReaderRef(pub TrackClipReaderHandle);

#[derive(Component, Debug, Clone, Copy)]
pub struct TrackClipReaderNode(pub tutti::NodeId);

// ---------------------------------------------------------------------------
// TrackClipReaderUnit — the AudioUnit.
// ---------------------------------------------------------------------------

pub struct TrackClipReaderUnit {
    clips: Vec<ClipSlot>,
    rx: Receiver<ClipCommand>,
}

impl TrackClipReaderUnit {
    pub fn new() -> (Self, TrackClipReaderHandle) {
        let (tx, rx) = bounded(COMMAND_CAPACITY);
        let handle = TrackClipReaderHandle { tx };
        let unit = Self {
            clips: Vec::new(),
            rx,
        };
        (unit, handle)
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
                    });
                }
                ClipCommand::Remove(id) => {
                    self.clips.retain(|s| s.id != id);
                }
                ClipCommand::ReplaceWave { id, wave } => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        slot.sampler.set_wave(wave);
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
            }
        }
    }

    #[inline]
    fn read_clip_sample(slot: &ClipSlot, pos: f64) -> (f32, f32) {
        if slot.reverse {
            let len = slot.sampler.duration_samples() as f64;
            let reversed = (len - 1.0 - pos).max(0.0);
            slot.sampler.get_sample(reversed)
        } else {
            slot.sampler.get_sample(pos)
        }
    }
}

impl Clone for TrackClipReaderUnit {
    fn clone(&self) -> Self {
        let (_tx, rx) = bounded(COMMAND_CAPACITY);
        Self {
            clips: self.clips.iter().map(|s| ClipSlot {
                id: s.id,
                sampler: s.sampler.clone(),
                reverse: s.reverse,
            }).collect(),
            rx,
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
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti::core::SampleRate) {
        for slot in &mut self.clips {
            slot.sampler.set_sample_rate(sample_rate);
        }
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        self.drain_commands();

        if output.len() < 2 {
            return;
        }

        let mut left = 0.0_f32;
        let mut right = 0.0_f32;

        for slot in &self.clips {
            if let Some(pos) = slot.sampler.transport_sample_position() {
                let (l, r) = Self::read_clip_sample(slot, pos);
                left += l;
                right += r;
            }
        }

        output[0] = left;
        output[1] = right;
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        self.drain_commands();

        // Zero the output first.
        for i in 0..size {
            output.set_f32(0, i, 0.0);
            output.set_f32(1, i, 0.0);
        }

        for slot in &self.clips {
            let Some(start_pos) = slot.sampler.transport_sample_position() else {
                continue;
            };
            let advance = (slot.sampler.speed() * slot.sampler.src_ratio()) as f64;
            for i in 0..size {
                let pos = start_pos + i as f64 * advance;
                let (l, r) = Self::read_clip_sample(slot, pos);
                output.set_f32(0, i, output.at_f32(0, i) + l);
                output.set_f32(1, i, output.at_f32(1, i) + r);
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
        std::mem::size_of::<Self>()
            + self.clips.len() * std::mem::size_of::<ClipSlot>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tutti::core::TransportReader;

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
        fn tempo(&self) -> tutti::core::Bpm {
            tutti::core::Bpm::new(f64::from_bits(self.tempo.load(Ordering::Relaxed)))
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
            let sampler = SamplerUnit::with_transport(wave.clone(), transport.clone(), 0.0, None);
            handle.send(ClipCommand::Add {
                id: SlotId(i),
                sampler,
                reverse: false,
            });
        }

        // Tick once to drain commands, then read.
        let mut out_3 = [0.0f32; 2];
        unit.tick(&[], &mut out_3);

        // Compare with a single clip.
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

        // Drain the command.
        let mut out = [0.0f32; 2];
        unit.tick(&[], &mut out);

        let mut cloned = unit.clone();
        let mut out_clone = [0.0f32; 2];
        cloned.tick(&[], &mut out_clone);

        assert!(out_clone[0] != 0.0, "cloned unit should have the clip");
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
}
