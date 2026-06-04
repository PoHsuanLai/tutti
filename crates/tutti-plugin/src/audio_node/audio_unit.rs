//! fundsp trait impls for [`PluginClient`]. Dual f32/f64 dispatch lives
//! here so the core struct + API in `mod.rs` stays focused.

use super::signal::route_with_latency;
use super::PluginClient;
use tutti_midi_types::{MidiTarget, MidiUnitId};
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame, F64};

impl AudioUnit for PluginClient {
    fn inputs(&self) -> usize {
        self.io_ref().inputs
    }

    fn outputs(&self) -> usize {
        self.io_ref().outputs
    }

    fn reset(&mut self) {
        self.io_mut().reset();
        self.midi_mut().reset_sample_pos();
        let _ = self.bridge_ref().reset_rt();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.io_mut().reset();
        self.midi_mut().reset_sample_pos();
        let _ = self.bridge_ref().set_sample_rate_rt(sample_rate);
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.io_mut().write::<f32>(input);
        if self.io_ref().should_flush() {
            let midi = self.midi_mut().drain_for_tick().clone();
            let bridge = self.bridge_ref().clone();
            self.io_mut().flush::<f32>(&bridge, midi);
        }
        self.io_mut().read::<f32>(output);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let midi = self.midi_mut().drain_for_process(size).clone();
        let bridge = self.bridge_ref().clone();
        self.io_mut()
            .process::<f32>(&bridge, size, input, output, midi);
    }

    fn get_id(&self) -> u64 {
        crate::node_id::PLUGIN_CLIENT_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let io = self.io_ref();
        route_with_latency(
            io.inputs,
            io.outputs,
            PluginClient::latency(self) as f64,
            input,
        )
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl AudioUnit<F64> for PluginClient {
    fn inputs(&self) -> usize {
        self.io_ref().inputs
    }

    fn outputs(&self) -> usize {
        self.io_ref().outputs
    }

    fn reset(&mut self) {
        self.io_mut().reset();
        self.midi_mut().reset_sample_pos();
        let _ = self.bridge_ref().reset_rt();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.io_mut().reset();
        self.midi_mut().reset_sample_pos();
        let _ = self.bridge_ref().set_sample_rate_rt(sample_rate);
    }

    fn tick(&mut self, input: &[f64], output: &mut [f64]) {
        self.io_mut().write::<f64>(input);
        if self.io_ref().should_flush() {
            let midi = self.midi_mut().drain_for_tick().clone();
            let bridge = self.bridge_ref().clone();
            self.io_mut().flush::<f64>(&bridge, midi);
        }
        self.io_mut().read::<f64>(output);
    }

    fn process(&mut self, size: usize, input: &BufferRef<F64>, output: &mut BufferMut<F64>) {
        let midi = self.midi_mut().drain_for_process(size).clone();
        let bridge = self.bridge_ref().clone();
        self.io_mut()
            .process::<f64>(&bridge, size, input, output, midi);
    }

    fn get_id(&self) -> u64 {
        crate::node_id::PLUGIN_CLIENT_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let io = self.io_ref();
        route_with_latency(
            io.inputs,
            io.outputs,
            PluginClient::latency(self) as f64,
            input,
        )
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl MidiTarget for PluginClient {
    fn midi_unit_id(&self) -> MidiUnitId {
        self.midi_ref().unit_id()
    }
}
