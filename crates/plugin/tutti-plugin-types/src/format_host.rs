//! The fine-grained plugin-instance capability traits every format loader
//! implements, and the [`PluginInstance`] bundle that composes them.
//!
//! A loaded plugin (VST2 / VST3 / CLAP / AU) is a *subprocess-local* object:
//! it lives entirely inside `tutti-plugin-server`, reached through
//! `Plugin::instance_mut()`, and never crosses the IPC wire. Its surface used
//! to be one 13-method god-trait; it is now split along the real capability
//! axis, one trait per concern, so a loader implements only the capabilities it
//! has and a consumer depends only on the capability it uses.
//!
//! Note the audio methods here ([`PluginAudio`]) are the *subprocess* render
//! path — distinct from the host-side `AudioUnit` node (`PluginClient`) on the
//! other end of the wire. Both are "process a block", but they are different
//! objects in different processes, so this is not a duplicate of `AudioUnit`.

use crate::{
    AudioBufferMut, EditorSize, LoadedPlugin, ParameterInfo, PluginDescriptor, ProcessContext,
    ProcessOutput, Result, WindowHandle,
};

/// Catalog identity + load-time engine-wiring snapshot.
///
/// Static identity (name, vendor, native class, has-editor) is on
/// [`descriptor`](Self::descriptor); per-bus widths / latency / f64 support on
/// [`loaded`](Self::loaded). Both are snapshots of what the plugin reported at
/// load time — pure `&self` queries with no live-plugin analogue on a fundsp
/// node (`AudioUnit::get_id()` is a shared *type* tag, not per-instance
/// identity).
pub trait PluginMeta {
    fn descriptor(&self) -> &PluginDescriptor;
    fn loaded(&self) -> &LoadedPlugin;
}

/// The subprocess audio render path.
///
/// The loader-side counterpart of the host-side `AudioUnit` node: same "render
/// one block" job, different object across the IPC boundary.
pub trait PluginAudio: Send {
    /// Process one audio block. The buffer carries the negotiated sample
    /// format (f32 or f64) as a tagged enum, so the trait stays
    /// dyn-compatible while implementations branch once and delegate into a
    /// single generic inner body.
    fn process(
        &mut self,
        buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
    ) -> Result<ProcessOutput>;

    fn set_sample_rate(&mut self, rate: f64);
}

/// Parameter enumeration, read, and write.
///
/// A fundsp node has no parameter *catalog* ([`get_parameter_list`](Self::get_parameter_list)
/// returns id/name/range/default/unit/flags with no node analogue), so this
/// stays plugin-specific.
pub trait PluginParams {
    /// Parameter value in the format's native convention: normalized 0..1 for
    /// VST2/VST3/AU, but the plugin's **native plain range** for CLAP (CLAP has
    /// no normalization concept). A consumer that needs a normalized value must
    /// scale by the `min_value`/`max_value` carried on [`ParameterInfo`].
    fn get_parameter(&self, id: u32) -> f64;

    /// See [`get_parameter`](Self::get_parameter) for the value convention
    /// (normalized for VST2/VST3/AU, native plain range for CLAP).
    fn set_parameter(&mut self, id: u32, value: f64);

    /// Push the host [`AutomationMode`](crate::AutomationMode) to the plugin.
    /// Fire-and-forget; the default no-op covers formats without an
    /// automation-state concept. A format that supports it (VST3's
    /// `IAutomationState`) encodes the mode onto its own ABI at the FFI edge.
    fn set_automation_state(&mut self, _mode: crate::AutomationMode) {}

    fn get_parameter_list(&self) -> Vec<ParameterInfo>;
}

/// Opaque preset-chunk save/load. No fundsp node has serializable opaque state,
/// so this is genuinely irreducible.
pub trait PluginState: Send {
    fn get_state(&mut self) -> Result<Vec<u8>>;
    fn set_state(&mut self, data: &[u8]) -> Result<()>;
}

/// The subprocess-side editor hooks.
///
/// Distinct from the host-side `PluginEditor` (the second, editor-only dlopen
/// in the main process): this is the editor surface a loader exposes *from
/// inside* the plugin-server subprocess. Only the in-process VST2 host drives a real
/// editor here; the subprocess-hosted formats run their editor on the platform
/// GUI toolkit's own run loop and inherit the [`editor_idle`](Self::editor_idle)
/// default no-op.
pub trait PluginEditorHost {
    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize>;
    fn close_editor(&mut self);
    /// Pump one editor idle tick. Only the in-process VST2 host needs this (its
    /// `AEffect` editor is driven by host-timer idle calls); everything else
    /// inherits this default no-op.
    fn editor_idle(&mut self) {}
}

/// A loaded plugin instance: the full capability bundle a format loader
/// implements.
///
/// This is a marker supertrait over the fine-grained capabilities, with a
/// blanket impl — a loader implements the small traits and gets
/// `PluginInstance` for free, and a consumer that needs "the whole plugin"
/// (the session dispatch) depends on this one bound. Consumers that need only
/// one capability should depend on that trait alone.
pub trait PluginInstance:
    PluginMeta + PluginAudio + PluginParams + PluginState + PluginEditorHost + Send
{
}

impl<T> PluginInstance for T where
    T: PluginMeta + PluginAudio + PluginParams + PluginState + PluginEditorHost + Send
{
}
