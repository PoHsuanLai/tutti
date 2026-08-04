//! fundsp trait impls for [`PluginClient`]. Dual f32/f64 dispatch lives
//! here so the core struct + API in `mod.rs` stays focused.

use super::batcher::PIPELINE_LATENCY_FRAMES;
use super::PluginClient;
use crate::util::node::route_with_latency;
use tutti_core::{AudioUnit, BufferMut, BufferRef, SignalFrame, F64};
use tutti_midi_types::MidiUnitId;

impl AudioUnit for PluginClient {
    fn inputs(&self) -> usize {
        self.io_ref().inputs
    }

    fn outputs(&self) -> usize {
        self.io_ref().outputs
    }

    fn reset(&mut self) {
        self.io_mut().reset();
        let _ = self.bridge_ref().reset_rt();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.io_mut().reset();
        self.restamp_source_rates(sample_rate);
        // `.get()` here and nowhere earlier: `set_sample_rate_rt` puts the rate
        // on the IPC wire, which is where the types stop (#105/#108). This used
        // to unwrap on entry, ten hops before the boundary that needed it.
        let _ = self.bridge_ref().set_sample_rate_rt(sample_rate.get());
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        self.io_mut().write::<f32>(input);
        if self.io_ref().should_flush() {
            let payload = self.build_block_payload(1);
            self.flush_batch::<f32>(payload);
        }
        self.io_mut().read::<f32>(output);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let payload = self.build_block_payload(size);
        self.process_block::<f32>(size, input, output, payload);
    }

    fn get_id(&self) -> u64 {
        crate::util::node::node_id::PLUGIN_CLIENT_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let io = self.io_ref();
        // The plugin's own reported latency PLUS the block the pipeline holds.
        // Out-of-process audio is submitted now and collected next block, so a
        // sample entering here leaves one block later than the plugin alone
        // would account for. Declaring it is what turns that into compensated
        // delay rather than audible drift — an out-of-process plugin on a
        // parallel path would otherwise arrive a block late against its dry
        // twin, which is the classic comb-filter smear.
        //
        // `AudioUnit::latency()` derives from `route()` and `LatencyGraph for
        // Net` calls it, so this addition alone reaches `latency::plan`.
        route_with_latency(
            io.inputs,
            io.outputs,
            (PluginClient::latency(self) + PIPELINE_LATENCY_FRAMES).get() as f64,
            input,
        )
    }

    /// What the plugin currently reports for its tail.
    ///
    /// The format loaders decode each format's own answer into
    /// [`Tail`](tutti_plugin_types::PluginTail) — AU's seconds, the `u32::MAX`
    /// sentinel CLAP and VST3 share, VST2's absence of a query. Nothing is
    /// re-interpreted here: a plugin that said nothing stays `Unknown` rather
    /// than becoming a zero.
    ///
    /// Reads the live cell, not the value captured at load, so a CLAP plugin
    /// whose decay is raised at runtime reports the new tail. The other three
    /// formats have no runtime signal, so for them the cell never moves off its
    /// load-time value.
    fn tail(&mut self) -> tutti_plugin_types::PluginTail {
        PluginClient::tail(self)
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
        let _ = self.bridge_ref().reset_rt();
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        self.io_mut().reset();
        self.restamp_source_rates(sample_rate);
        // `.get()` here and nowhere earlier: `set_sample_rate_rt` puts the rate
        // on the IPC wire, which is where the types stop (#105/#108). This used
        // to unwrap on entry, ten hops before the boundary that needed it.
        let _ = self.bridge_ref().set_sample_rate_rt(sample_rate.get());
    }

    fn tick(&mut self, input: &[f64], output: &mut [f64]) {
        self.io_mut().write::<f64>(input);
        if self.io_ref().should_flush() {
            let payload = self.build_block_payload(1);
            self.flush_batch::<f64>(payload);
        }
        self.io_mut().read::<f64>(output);
    }

    fn process(&mut self, size: usize, input: &BufferRef<F64>, output: &mut BufferMut<F64>) {
        let payload = self.build_block_payload(size);
        self.process_block::<f64>(size, input, output, payload);
    }

    fn get_id(&self) -> u64 {
        crate::util::node::node_id::PLUGIN_CLIENT_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn route(&mut self, input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let io = self.io_ref();
        // The plugin's own reported latency PLUS the block the pipeline holds.
        // Out-of-process audio is submitted now and collected next block, so a
        // sample entering here leaves one block later than the plugin alone
        // would account for. Declaring it is what turns that into compensated
        // delay rather than audible drift — an out-of-process plugin on a
        // parallel path would otherwise arrive a block late against its dry
        // twin, which is the classic comb-filter smear.
        //
        // `AudioUnit::latency()` derives from `route()` and `LatencyGraph for
        // Net` calls it, so this addition alone reaches `latency::plan`.
        route_with_latency(
            io.inputs,
            io.outputs,
            (PluginClient::latency(self) + PIPELINE_LATENCY_FRAMES).get() as f64,
            input,
        )
    }

    /// What the plugin currently reports — see the `f32` impl.
    fn tail(&mut self) -> tutti_plugin_types::PluginTail {
        PluginClient::tail(self)
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

impl PluginClient {
    /// This unit's MIDI routing address.
    pub fn midi_unit_id(&self) -> MidiUnitId {
        self.midi_ref().unit_id()
    }
}
