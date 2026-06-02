//! Audio-thread synth node.
//!
//! [`Synth`] is a newtype around `InferenceNode<MidiBlock, ParamSink>`.
//!
//! ```text
//! MIDI events  ─►  MidiBlock.ingest
//!                    │
//!                    ▼  on note on/off:
//!                 MidiFeatures buffer (arc_pool)
//!                    │
//!                    ▼  via Event::Req
//!                 Engine.forward
//!                    │
//!                    ▼  via Response::Params
//!                 ParamSink.render  →  amplitudes[0] · sin(phase)
//! ```
//!
//! | Leaf | Role |
//! | ---- | ---- |
//! | [`MidiBlock`] | [`Trigger`] — MIDI receiver + [`MidiState`] + `arc_pool`. Yields a feature buffer on note-on/off. |
//! | [`ParamSink`] | [`Sink`] — owns the [`ControlParams`] receiver and the sine-osc state. |
//!
//! # Feature gate
//!
//! `#[cfg(feature = "midi")]`. Without the feature, `Synth` and `synth_node`
//! are absent from the public API.

#![cfg(feature = "midi")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crossbeam_channel::{bounded, Receiver, Sender};
use tutti_core::midi::{MidiSource, MidiTarget, MidiUnitId};
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame};
use tutti_midi::semantic::SemanticEvent;
use tutti_midi::ump::MidiEvent;
use tutti_midi_runtime::{MidiEventSlot, MidiReceiver, MidiSender};

use crate::ipc::{self, ArcPool, ControlParams, Event, Response, Shape};
use crate::model_id::ModelId;
use crate::node::{InferenceNode, Sink, Trigger};

pub const MIDI_FEATURE_COUNT: usize = 12;
const MIDI_POLL_CAPACITY: usize = 256;
const POOL_SIZE: usize = 4;
const PARAM_CHANNEL_CAPACITY: usize = 16;

/// Per-voice MIDI state accumulator.
///
/// Feature layout (see [`Self::to_features`]):
/// `[pitch_hz, loudness, pitch_bend, mod_wheel, brightness, expression,
/// channel_pressure, sustain, note_number, velocity_unit, bend_range, reserved]`.
#[derive(Debug, Clone)]
pub struct MidiState {
    pub note: Option<u8>,
    /// Velocity in `[0.0, 1.0]`. Zero means note-off.
    pub velocity: f32,
    pub pitch_bend: f32,
    pub pitch_bend_range: f32,
    pub mod_wheel: f32,
    pub brightness: f32,
    pub expression: f32,
    pub channel_pressure: f32,
    pub sustain: bool,
}

impl Default for MidiState {
    fn default() -> Self {
        Self {
            note: None,
            velocity: 0.0,
            pitch_bend: 0.0,
            pitch_bend_range: 2.0,
            mod_wheel: 0.0,
            brightness: 0.5,
            expression: 1.0,
            channel_pressure: 0.0,
            sustain: false,
        }
    }
}

impl MidiState {
    /// Fold `event` into the state. Returns `true` when the event is a
    /// note-on/off (the kind of trigger that should resubmit inference).
    pub fn apply(&mut self, event: &MidiEvent) -> bool {
        let Some(sem) = tutti_midi::decode(event) else {
            return false;
        };
        match sem {
            SemanticEvent::NoteOn { note, velocity, .. } => {
                self.note = Some(note);
                self.velocity = velocity;
                true
            }
            SemanticEvent::NoteOff { .. } => {
                self.velocity = 0.0;
                true
            }
            SemanticEvent::PitchBend { value, .. } => {
                self.pitch_bend = value;
                false
            }
            SemanticEvent::ChannelPressure { value, .. } => {
                self.channel_pressure = value;
                false
            }
            SemanticEvent::KeyPressure { note, value, .. } => {
                if self.note == Some(note) {
                    self.channel_pressure = value;
                }
                false
            }
            SemanticEvent::ControlChange { cc, value, .. } => {
                self.apply_cc(cc, value);
                false
            }
            _ => false,
        }
    }

    fn apply_cc(&mut self, cc_num: u8, value: f32) {
        use tutti_midi::cc;
        match cc_num {
            cc::MOD_WHEEL => self.mod_wheel = value,
            cc::EXPRESSION => self.expression = value,
            cc::SUSTAIN => self.sustain = value >= 0.5,
            cc::BRIGHTNESS => self.brightness = value,
            _ => {}
        }
    }

    pub fn pitch_hz(&self) -> f32 {
        let note = f32::from(self.note.unwrap_or(60));
        let bent = note + self.pitch_bend * self.pitch_bend_range;
        440.0 * 2.0_f32.powf((bent - 69.0) / 12.0)
    }

    pub fn loudness(&self) -> f32 {
        self.velocity * self.expression
    }

    pub fn to_features(&self) -> [f32; MIDI_FEATURE_COUNT] {
        [
            self.pitch_hz(),
            self.loudness(),
            self.pitch_bend,
            self.mod_wheel,
            self.brightness,
            self.expression,
            self.channel_pressure,
            if self.sustain { 1.0 } else { 0.0 },
            f32::from(self.note.unwrap_or(0)),
            self.velocity,
            self.pitch_bend_range,
            0.0,
        ]
    }
}

/// MIDI [`Trigger`]: polls a MIDI receiver, folds events into [`MidiState`],
/// yields a feature buffer for inference whenever a note-on/off arrives.
///
/// Owns the MIDI receiver and an optional offline-export source override.
/// Allocates feature buffers from a round-robin `arc_pool`.
pub struct MidiBlock {
    midi_unit_id: MidiUnitId,
    state: MidiState,
    receiver: MidiReceiver,
    sender: MidiSender,
    source_override: Option<Box<dyn MidiSource>>,
    poll_buffer: [MidiEvent; MIDI_POLL_CAPACITY],
    pool: ArcPool,
}

impl MidiBlock {
    pub fn new() -> Self {
        let midi_unit_id = MidiUnitId::next();
        let (sender, receiver) = MidiEventSlot::pair(midi_unit_id);
        Self {
            midi_unit_id,
            state: MidiState::default(),
            receiver,
            sender,
            source_override: None,
            poll_buffer: [MidiEvent::noop(); MIDI_POLL_CAPACITY],
            pool: ArcPool::new(POOL_SIZE, MIDI_FEATURE_COUNT),
        }
    }

    pub fn midi_unit_id(&self) -> MidiUnitId {
        self.midi_unit_id
    }

    pub fn sender(&self) -> MidiSender {
        self.sender.clone()
    }

    pub fn set_source(&mut self, source: Box<dyn MidiSource>) {
        self.source_override = Some(source);
    }
}

impl Default for MidiBlock {
    fn default() -> Self {
        Self::new()
    }
}

impl Trigger for MidiBlock {
    type Input<'a> = ();

    fn shape(&self) -> Shape {
        Shape::new(1, MIDI_FEATURE_COUNT)
    }

    fn ingest(&mut self, _: ()) -> Option<Arc<[f32]>> {
        // Neural triggers don't run inside an `AudioUnit::process`, so no
        // block-start sample is available. Pass zero — sources that
        // schedule by sample (snapshot/clip readers) will degrade to
        // "all events at frame_offset = 0", which matches the
        // pre-sample-accuracy behaviour for this code path.
        let count = match &self.source_override {
            Some(src) => src.poll_into(self.midi_unit_id, 0, 0, &mut self.poll_buffer),
            None => self.receiver.poll_into(&mut self.poll_buffer),
        };
        let mut should_submit = false;
        for i in 0..count {
            let event = self.poll_buffer[i];
            should_submit |= self.state.apply(&event);
        }
        if should_submit {
            self.pool.fill(&self.state.to_features())
        } else {
            None
        }
    }
}

/// Param [`Sink`]: receives [`ControlParams`] from the engine, drives a sine
/// oscillator on the audio thread.
pub struct ParamSink {
    rx: Receiver<ControlParams>,
    tx: Sender<ControlParams>,
    current: ControlParams,
    buffer_size: usize,
    sample_rate: f32,
    phase: f32,
}

impl ParamSink {
    pub fn new(sample_rate: f32, buffer_size: usize) -> Self {
        let (tx, rx) = bounded::<ControlParams>(PARAM_CHANNEL_CAPACITY);
        Self {
            rx,
            tx,
            current: ControlParams {
                f0: vec![440.0; buffer_size],
                amplitudes: vec![0.0; buffer_size],
            },
            buffer_size,
            sample_rate,
            phase: 0.0,
        }
    }

    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        self.sample_rate = sample_rate;
    }

    pub fn reset_phase(&mut self) {
        self.phase = 0.0;
    }

    /// Drain pending param updates, keeping the most recent.
    fn poll(&mut self) {
        while let Ok(p) = self.rx.try_recv() {
            self.current = p;
        }
    }

    pub fn current(&self) -> &ControlParams {
        &self.current
    }

    /// Render `size` samples into a stereo `BufferMut` at audio-block
    /// granularity. Used by `Synth::process` to batch.
    pub fn render_block(&mut self, size: usize, output: &mut BufferMut) {
        self.poll();
        let p = &self.current;
        let n = size.min(p.f0.len()).min(p.amplitudes.len());
        if n == 0 {
            for i in 0..size {
                output.set_f32(0, i, 0.0);
                output.set_f32(1, i, 0.0);
            }
            return;
        }
        let two_pi = 2.0 * std::f32::consts::PI;
        let mut phase = self.phase;
        for i in 0..n {
            let sample = p.amplitudes[i] * phase.sin();
            phase += (p.f0[i] / self.sample_rate) * two_pi;
            if phase >= two_pi {
                phase -= two_pi;
            }
            output.set_f32(0, i, sample);
            output.set_f32(1, i, sample);
        }
        self.phase = phase;
        for i in n..size {
            output.set_f32(0, i, 0.0);
            output.set_f32(1, i, 0.0);
        }
    }
}

impl Sink for ParamSink {
    fn response(&mut self) -> Response {
        Response::Params {
            tx: self.tx.clone(),
            buffer_size: self.buffer_size,
        }
    }

    fn render(&mut self, output: &mut [f32]) {
        self.poll();
        let p = &self.current;
        if p.f0.is_empty() || p.amplitudes.is_empty() {
            output.fill(0.0);
            return;
        }
        let two_pi = 2.0 * std::f32::consts::PI;
        let sample = p.amplitudes[0] * self.phase.sin();
        self.phase += (p.f0[0] / self.sample_rate) * two_pi;
        if self.phase >= two_pi {
            self.phase -= two_pi;
        }
        if output.len() >= 2 {
            output[0] = sample;
            output[1] = sample;
        }
    }
}

/// Neural-synth audio unit. Zero inputs, stereo out. MIDI in via
/// [`Self::midi_sender`] (or [`Self::set_midi_source`] for offline export).
///
/// `latency_samples` is reported to the graph's PDC via
/// [`AudioUnit::latency`] so downstream nodes align around the model's
/// constant processing delay. The value lives behind an
/// [`Arc<AtomicUsize>`] so callers can update it after construction via
/// [`Synth::latency_handle`] — clones share the same cell.
pub struct Synth {
    inner: InferenceNode<MidiBlock, ParamSink>,
    sample_rate: f32,
    buffer_size: usize,
    latency_samples: Arc<AtomicUsize>,
}

impl Synth {
    pub fn midi_sender(&self) -> MidiSender {
        self.inner.trigger.sender()
    }

    pub fn set_midi_source(&mut self, source: Box<dyn MidiSource>) {
        self.inner.trigger.set_source(source);
    }

    /// Handle for updating this node's reported latency without rebuilding.
    /// Mirrors [`Effect::latency_handle`](crate::Effect::latency_handle).
    pub fn latency_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.latency_samples)
    }
}

impl AudioUnit for Synth {
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        2
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        self.inner.step((), output);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        // Trigger once per block (the MIDI receiver is drained inside).
        if let Some(buf) = self.inner.trigger.ingest(()) {
            ipc::submit(
                &self.inner.tx,
                crate::ipc::Request {
                    id: self.inner.id,
                    input: buf,
                    shape: self.inner.trigger.shape(),
                    resp: self.inner.sink.response(),
                },
            );
        }
        self.inner.sink.render_block(size, output);
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate as f32;
        self.inner.sink.set_sample_rate(self.sample_rate);
    }

    fn reset(&mut self) {
        self.inner.sink.reset_phase();
    }

    fn get_id(&self) -> u64 {
        tutti_core::node_id::NEURAL_SYNTH_ID
    }

    fn latency(&mut self) -> Option<f64> {
        Some(self.latency_samples.load(Ordering::Acquire) as f64)
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(2)
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn footprint(&self) -> usize {
        let p = self.inner.sink.current();
        std::mem::size_of::<Self>()
            + p.f0.len() * std::mem::size_of::<f32>()
            + p.amplitudes.len() * std::mem::size_of::<f32>()
    }
}

impl Clone for Synth {
    /// Share the latency cell with the clone; the rest is rebuilt through
    /// a fresh [`InferenceNode`] + MIDI receiver.
    fn clone(&self) -> Self {
        let trigger = MidiBlock::new();
        let sink = ParamSink::new(self.sample_rate, self.buffer_size);
        Synth {
            inner: InferenceNode::new(self.inner.id, self.inner.tx.clone(), trigger, sink),
            sample_rate: self.sample_rate,
            buffer_size: self.buffer_size,
            latency_samples: Arc::clone(&self.latency_samples),
        }
    }
}

impl MidiTarget for Synth {
    fn midi_unit_id(&self) -> MidiUnitId {
        self.inner.trigger.midi_unit_id()
    }
}

/// Build a [`Synth`] driven by `id`. Zero inputs, stereo output.
///
/// Inference happens off-thread; the audio thread advances phase using the
/// last-seen [`ControlParams`]. Push MIDI events via [`Synth::midi_sender`].
///
/// `latency_samples` is the constant processing delay reported to the
/// graph's PDC — typically
/// [`loaded.report.latency_samples(engine.sample_rate())`](crate::ProbeReport::latency_samples).
pub fn synth_node(
    id: ModelId,
    sample_rate: f32,
    buffer_size: usize,
    latency_samples: usize,
    tx: Sender<Event>,
) -> Synth {
    let trigger = MidiBlock::new();
    let sink = ParamSink::new(sample_rate, buffer_size);
    Synth {
        inner: InferenceNode::new(id, tx, trigger, sink),
        sample_rate,
        buffer_size,
        latency_samples: Arc::new(AtomicUsize::new(latency_samples)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded;
    use tutti_midi::convert::{
        midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2,
    };
    use tutti_midi::ump::MidiEvent;

    fn ev_note_on(channel: u8, note: u8, vel: u8) -> MidiEvent {
        MidiEvent::note_on(0, channel, note, midi1_velocity_to_midi2(vel))
    }
    fn ev_note_off(channel: u8, note: u8) -> MidiEvent {
        MidiEvent::note_off(0, channel, note, 0)
    }
    fn ev_cc(channel: u8, cc_num: u8, value: u8) -> MidiEvent {
        MidiEvent::cc(0, channel, cc_num, midi1_cc_to_midi2(value))
    }
    fn ev_bend(channel: u8, bend14: u16) -> MidiEvent {
        MidiEvent::pitch_bend(0, channel, midi1_pitch_bend_to_midi2(bend14))
    }
    fn ev_channel_pressure(channel: u8, pressure: u8) -> MidiEvent {
        MidiEvent::channel_pressure(0, channel, midi1_cc_to_midi2(pressure))
    }

    #[test]
    fn test_default_state() {
        let s = MidiState::default();
        assert_eq!(s.note, None);
        assert_eq!(s.velocity, 0.0);
        assert_eq!(s.pitch_bend, 0.0);
        assert_eq!(s.loudness(), 0.0);
    }

    #[test]
    fn test_note_on() {
        let mut s = MidiState::default();
        assert!(s.apply(&ev_note_on(0, 69, 100)));
        assert_eq!(s.note, Some(69));
        assert!((s.velocity - 100.0 / 127.0).abs() < 0.01);
        assert!((s.pitch_hz() - 440.0).abs() < 0.1);
        assert!((s.loudness() - 100.0 / 127.0).abs() < 0.01);
    }

    #[test]
    fn test_note_off() {
        let mut s = MidiState::default();
        s.apply(&ev_note_on(0, 60, 80));
        assert!(s.apply(&ev_note_off(0, 60)));
        assert_eq!(s.velocity, 0.0);
        assert_eq!(s.loudness(), 0.0);
        assert_eq!(s.note, Some(60));
    }

    #[test]
    fn test_pitch_bend() {
        let mut s = MidiState::default();
        s.apply(&ev_note_on(0, 69, 100));
        assert!(!s.apply(&ev_bend(0, 16383)));
        assert!((s.pitch_bend - 1.0).abs() < 0.01);
        assert!((s.pitch_hz() - 493.88).abs() < 1.0);
        s.apply(&ev_bend(0, 8192));
        assert!(s.pitch_bend.abs() < 0.01);
    }

    #[test]
    fn test_cc_mod_wheel() {
        let mut s = MidiState::default();
        assert!(!s.apply(&ev_cc(0, 1, 64)));
        assert!((s.mod_wheel - 64.0 / 127.0).abs() < 0.01);
    }

    #[test]
    fn test_cc_expression_affects_loudness() {
        let mut s = MidiState::default();
        s.apply(&ev_note_on(0, 60, 100));
        s.apply(&ev_cc(0, 11, 64));
        let expected = (100.0 / 127.0) * (64.0 / 127.0);
        assert!((s.loudness() - expected).abs() < 0.01);
    }

    #[test]
    fn test_cc_sustain() {
        let mut s = MidiState::default();
        s.apply(&ev_cc(0, 64, 127));
        assert!(s.sustain);
        s.apply(&ev_cc(0, 64, 0));
        assert!(!s.sustain);
    }

    #[test]
    fn test_channel_pressure() {
        let mut s = MidiState::default();
        s.apply(&ev_channel_pressure(0, 100));
        assert!((s.channel_pressure - 100.0 / 127.0).abs() < 0.01);
    }

    #[test]
    fn test_to_features_layout() {
        let mut s = MidiState::default();
        s.apply(&ev_note_on(0, 60, 100));
        let f = s.to_features();
        assert_eq!(f.len(), MIDI_FEATURE_COUNT);
        assert!((f[0] - 261.63).abs() < 1.0);
        assert!(f[1] > 0.0);
        assert_eq!(f[8], 60.0);
        assert!((f[9] - 100.0 / 127.0).abs() < 0.01);
    }

    #[test]
    fn test_midi_block_no_event_no_submit() {
        let mut t = MidiBlock::new();
        assert!(t.ingest(()).is_none());
    }

    #[test]
    fn test_midi_block_yields_on_note_on() {
        let mut t = MidiBlock::new();
        let sender = t.sender();
        sender.queue(&[ev_note_on(0, 60, 100)]);
        let buf = t.ingest(()).expect("note-on should yield features");
        assert_eq!(buf.len(), MIDI_FEATURE_COUNT);
    }

    #[test]
    fn test_param_sink_renders_silence_when_zero_amp() {
        let mut sink = ParamSink::new(44100.0, 512);
        let mut out = [1.0f32, 1.0];
        sink.render(&mut out);
        // Default amplitude is 0.0 → silence.
        assert_eq!(out, [0.0, 0.0]);
    }

    #[test]
    fn test_synth_node_io() {
        let (tx, _rx) = unbounded();
        let node = synth_node(ModelId::new(), 44100.0, 512, 512, tx);
        assert_eq!(node.inputs(), 0);
        assert_eq!(node.outputs(), 2);
    }

    #[test]
    fn test_synth_latency_reports_probe_samples() {
        let (tx, _rx) = unbounded();
        let mut node = synth_node(ModelId::new(), 44100.0, 512, 2048, tx);
        assert_eq!(node.latency(), Some(2048.0));
    }

    #[test]
    fn test_synth_latency_handle_updates_reported_latency() {
        let (tx, _rx) = unbounded();
        let mut node = synth_node(ModelId::new(), 44100.0, 512, 512, tx);
        assert_eq!(node.latency(), Some(512.0));
        node.latency_handle().store(3072, Ordering::Release);
        assert_eq!(node.latency(), Some(3072.0));
    }

    #[test]
    fn test_synth_clone_shares_latency_cell() {
        let (tx, _rx) = unbounded();
        let node = synth_node(ModelId::new(), 44100.0, 512, 512, tx);
        let handle = node.latency_handle();
        let mut cloned = node.clone();
        handle.store(8192, Ordering::Release);
        assert_eq!(cloned.latency(), Some(8192.0));
    }

    #[test]
    fn test_synth_node_param_update() {
        let (tx, _rx) = unbounded();
        let mut node = synth_node(ModelId::new(), 44100.0, 512, 512, tx);
        // Send through the sink's tx to simulate engine response.
        node.inner
            .sink
            .tx
            .send(ControlParams {
                f0: vec![220.0; 512],
                amplitudes: vec![0.5; 512],
            })
            .unwrap();
        node.inner.sink.poll();
        assert_eq!(node.inner.sink.current().f0[0], 220.0);
        assert_eq!(node.inner.sink.current().amplitudes[0], 0.5);
    }
}
