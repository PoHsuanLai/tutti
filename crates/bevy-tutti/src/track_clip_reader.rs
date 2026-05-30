//! Per-track clip reader: a single graph node that internally manages
//! all audio clip playback for one track.
//!
//! Replaces the old "one `SamplerUnit` graph node per clip + dynamic
//! `StereoSumUnit`" model. ECS systems send [`ClipCommand`]s through
//! a [`TrackClipReaderHandle`]; the unit drains them each audio buffer.
//!
//! Clips can be either in-memory (`SamplerUnit`) or disk-streaming
//! (`StreamingSamplerUnit`). The [`PlaybackUnit`] trait covers the
//! shared parameter surface (gain, speed, play/stop); the process
//! loop branches on [`ClipSampler`] for the fundamentally different
//! read models.

use std::sync::Arc;

use bevy_ecs::prelude::*;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use tutti::core::{AudioUnit, BufferMut, BufferRef, SignalFrame, TransportReader, Wave};
use tutti::sampler::stretch;
use tutti::sampler::{PlaybackUnit, SamplerUnit, StreamingSamplerUnit};

const COMMAND_CAPACITY: usize = 64;
const TRACK_CLIP_READER_ID: u64 = 0x_0000_0000_0000_DA03;

// ---------------------------------------------------------------------------
// Slot ID — opaque u128 so bevy-tutti stays independent of dawai-types.
// dawai-model converts ClipId ↔ SlotId at the boundary.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(pub u128);

// ---------------------------------------------------------------------------
// ClipSampler — in-memory or streaming playback backend.
// ---------------------------------------------------------------------------

pub enum ClipSampler {
    InMemory(SamplerUnit),
    Streaming {
        unit: StreamingSamplerUnit,
        channel_index: usize,
        start_beat: f64,
        duration_beats: Option<f64>,
    },
}

impl ClipSampler {
    fn as_playback_unit(&mut self) -> &mut dyn PlaybackUnit {
        match self {
            Self::InMemory(s) => s,
            Self::Streaming { unit, .. } => unit,
        }
    }

    fn as_audio_unit(&mut self) -> &mut dyn AudioUnit {
        match self {
            Self::InMemory(s) => s,
            Self::Streaming { unit, .. } => unit,
        }
    }
}

impl Clone for ClipSampler {
    fn clone(&self) -> Self {
        match self {
            Self::InMemory(s) => Self::InMemory(s.clone()),
            Self::Streaming {
                unit,
                channel_index,
                start_beat,
                duration_beats,
            } => Self::Streaming {
                unit: unit.clone(),
                channel_index: *channel_index,
                start_beat: *start_beat,
                duration_beats: *duration_beats,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// ClipSlot — one clip's playback state.
// ---------------------------------------------------------------------------

struct ClipSlot {
    id: SlotId,
    sampler: ClipSampler,
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
            if let ClipSampler::InMemory(ref sampler) = self.sampler {
                let unit = stretch::Unit::new(Box::new(sampler.clone()), self.sample_rate);
                unit.set_stretch_factor(self.stretch_factor);
                unit.set_pitch_cents(self.pitch_cents);
                self.stretch = Some(unit);
            }
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
    AddStreaming {
        id: SlotId,
        unit: StreamingSamplerUnit,
        channel_index: usize,
        start_beat: f64,
        duration_beats: Option<f64>,
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
                        sampler: ClipSampler::InMemory(sampler),
                        reverse,
                        stretch: None,
                        stretch_factor: 1.0,
                        pitch_cents: 0.0,
                        sample_rate: self.sample_rate,
                    });
                }
                ClipCommand::AddStreaming {
                    id,
                    unit,
                    channel_index,
                    start_beat,
                    duration_beats,
                } => {
                    self.clips.retain(|s| s.id != id);
                    self.clips.push(ClipSlot {
                        id,
                        sampler: ClipSampler::Streaming {
                            unit,
                            channel_index,
                            start_beat,
                            duration_beats,
                        },
                        reverse: false,
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
                        if let ClipSampler::InMemory(ref mut s) = slot.sampler {
                            s.set_wave(wave);
                            slot.rebuild_stretch();
                        }
                    }
                }
                ClipCommand::UpdatePlacement {
                    id,
                    start_beat,
                    duration_beats,
                } => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        match &mut slot.sampler {
                            ClipSampler::InMemory(s) => {
                                s.set_placement(start_beat, duration_beats);
                            }
                            ClipSampler::Streaming {
                                start_beat: sb,
                                duration_beats: db,
                                ..
                            } => {
                                *sb = start_beat;
                                *db = duration_beats;
                            }
                        }
                    }
                }
                ClipCommand::UpdateGain { id, gain } => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        slot.sampler.as_playback_unit().set_gain(gain);
                    }
                }
                ClipCommand::UpdateSpeed { id, speed } => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        slot.sampler.as_playback_unit().set_speed(speed);
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
                        if let ClipSampler::InMemory(ref mut s) = slot.sampler {
                            if looping {
                                s.set_loop_range(loop_start, loop_end, crossfade_samples);
                            } else {
                                s.clear_loop_range();
                                s.set_looping(false);
                            }
                        }
                    }
                }
                ClipCommand::ClearLoop(id) => {
                    if let Some(slot) = self.clips.iter_mut().find(|s| s.id == id) {
                        if let ClipSampler::InMemory(ref mut s) = slot.sampler {
                            s.clear_loop_range();
                            s.set_looping(false);
                        }
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

    fn is_streaming_clip_active(
        transport: &Option<Arc<dyn TransportReader>>,
        start_beat: f64,
        duration_beats: Option<f64>,
    ) -> bool {
        let Some(ref t) = transport else {
            return false;
        };
        if !t.is_playing() {
            return false;
        }
        let beat = t.current_beat();
        if beat < start_beat {
            return false;
        }
        if let Some(dur) = duration_beats {
            if beat >= start_beat + dur {
                return false;
            }
        }
        true
    }
}

impl Clone for TrackClipReaderUnit {
    fn clone(&self) -> Self {
        // `AudioUnit: DynClone`, so the graph (fundsp `Net`) may clone this
        // unit when it reallocates or swaps a node, then tick the clone and
        // drop the original. The clone therefore MUST keep receiving the
        // commands the ECS handle's `Sender` still feeds — so we share the
        // *same* `Receiver` rather than minting a fresh, dead channel.
        // `crossbeam` delivers each message to exactly one receiver, and the
        // graph only ever ticks one instance at a time, so there is no
        // double-drain.
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
            slot.sampler.as_audio_unit().reset();
            if let Some(ref mut s) = slot.stretch {
                s.reset();
            }
        }
    }

    fn set_sample_rate(&mut self, sample_rate: tutti::core::SampleRate) {
        self.sample_rate = sample_rate.get();
        for slot in &mut self.clips {
            slot.sampler.as_audio_unit().set_sample_rate(sample_rate);
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
            } else {
                match &mut slot.sampler {
                    ClipSampler::InMemory(ref sampler) => {
                        if let Some(pos) = sampler.transport_sample_position() {
                            let (l, r) = Self::read_clip_sample(sampler, slot.reverse, pos);
                            left += l;
                            right += r;
                        }
                    }
                    ClipSampler::Streaming {
                        ref mut unit,
                        start_beat,
                        duration_beats,
                        ..
                    } => {
                        if Self::is_streaming_clip_active(
                            &self.transport,
                            *start_beat,
                            *duration_beats,
                        ) {
                            let mut buf = [0.0f32; 2];
                            unit.tick(&[], &mut buf);
                            left += buf[0];
                            right += buf[1];
                        }
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
            if let Some(ref mut s) = slot.stretch {
                let mut tick_out = [0.0f32; 2];
                for i in 0..size {
                    s.tick(&[], &mut tick_out);
                    output.set_f32(0, i, output.at_f32(0, i) + tick_out[0]);
                    output.set_f32(1, i, output.at_f32(1, i) + tick_out[1]);
                }
            } else {
                match &mut slot.sampler {
                    ClipSampler::InMemory(ref sampler) => {
                        let Some(start_pos) = sampler.transport_sample_position() else {
                            continue;
                        };
                        let advance = (sampler.speed() * sampler.src_ratio()) as f64;
                        for i in 0..size {
                            let pos = start_pos + i as f64 * advance;
                            let (l, r) = Self::read_clip_sample(sampler, slot.reverse, pos);
                            output.set_f32(0, i, output.at_f32(0, i) + l);
                            output.set_f32(1, i, output.at_f32(1, i) + r);
                        }
                    }
                    ClipSampler::Streaming {
                        ref mut unit,
                        start_beat,
                        duration_beats,
                        ..
                    } => {
                        if !Self::is_streaming_clip_active(
                            &self.transport,
                            *start_beat,
                            *duration_beats,
                        ) {
                            continue;
                        }
                        let mut tick_out = [0.0f32; 2];
                        for i in 0..size {
                            unit.tick(&[], &mut tick_out);
                            output.set_f32(0, i, output.at_f32(0, i) + tick_out[0]);
                            output.set_f32(1, i, output.at_f32(1, i) + tick_out[1]);
                        }
                    }
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
