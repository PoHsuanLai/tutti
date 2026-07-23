//! The unified plugin-format host trait every format loader implements.

use crate::{
    AudioBufferMut, EditorSize, LoadedPlugin, ParameterInfo, PluginDescriptor, ProcessContext,
    ProcessOutput, Result, WindowHandle,
};

/// Unified interface for VST2, VST3, CLAP, and AU plugin instances,
/// implemented by each format's loader crate.
///
/// Static identity (name, vendor, native class, editor) goes through
/// [`descriptor`](Self::descriptor); engine-wiring data (per-bus widths,
/// latency, f64) through [`loaded`](Self::loaded). Both are snapshots of what
/// the plugin reported at load time. The trait's remaining methods are the
/// ones that need a live plugin reference.
pub trait PluginFormatHost: Send {
    fn descriptor(&self) -> &PluginDescriptor;

    fn loaded(&self) -> &LoadedPlugin;

    /// Process one audio block. The buffer carries the negotiated sample
    /// format (f32 or f64) as a tagged enum, so the trait stays
    /// dyn-compatible while implementations can branch once and delegate
    /// into a single generic inner body.
    fn process(
        &mut self,
        buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
    ) -> Result<ProcessOutput>;

    fn set_sample_rate(&mut self, rate: f64);

    /// Parameter value in the format's native convention: normalized 0..1 for
    /// VST2/VST3/AU, but the plugin's **native plain range** for CLAP (CLAP has
    /// no normalization concept). A consumer that needs a normalized value must
    /// scale by the `min_value`/`max_value` carried on [`ParameterInfo`].
    fn get_parameter(&self, id: u32) -> f64;

    /// See [`get_parameter`](Self::get_parameter) for the value convention
    /// (normalized for VST2/VST3/AU, native plain range for CLAP).
    fn set_parameter(&mut self, id: u32, value: f64);

    /// Push the host automation read/write state to the plugin. Fire-and-forget;
    /// the default no-op covers formats without an automation-state concept
    /// (only VST3's `IAutomationState` implements it).
    fn set_automation_state(&mut self, _state: i32) {}

    fn get_parameter_list(&self) -> Vec<ParameterInfo>;

    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize>;

    fn close_editor(&mut self);

    fn get_state(&mut self) -> Result<Vec<u8>>;

    fn set_state(&mut self, data: &[u8]) -> Result<()>;
}
