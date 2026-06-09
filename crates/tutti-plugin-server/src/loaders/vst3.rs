//! VST3 plugin loader using the `vst3-host` crate.

use std::path::Path;

use tutti_plugin::server::{
    BusLayout, ChordChanges, EditorSize, NoteExpressionChanges, NoteExpressionIntChanges,
    NoteExpressionTextChanges, NoteExpressionType, ParameterFlags, ParameterInfo, PluginInfo,
    ScaleChanges, WindowHandle,
};
use tutti_plugin::{BridgeError, LoadStage, Result};

use crate::loaders::common::params::{make_param_info, ParamCache};

pub use tutti_vst3_host;

/// Typed inner holds either a f32 or f64 instance, selected at activation time
/// based on plugin capabilities and the caller's preferred format.
enum VstInner {
    F32(tutti_vst3_host::Vst3Instance<f32>),
    F64(tutti_vst3_host::Vst3Instance<f64>),
}

/// Dispatch a shared expression over both inner variants (immutable).
macro_rules! vst_dispatch {
    ($self:expr, $inner:ident => $body:expr) => {
        match &$self.inner {
            VstInner::F32($inner) => $body,
            VstInner::F64($inner) => $body,
        }
    };
}

/// Dispatch a shared expression over both inner variants (mutable).
macro_rules! vst_dispatch_mut {
    ($self:expr, $inner:ident => $body:expr) => {
        match &mut $self.inner {
            VstInner::F32($inner) => $body,
            VstInner::F64($inner) => $body,
        }
    };
}

pub struct Vst3Instance {
    inner: VstInner,
    metadata: PluginInfo,
    param_cache: ParamCache,
    /// Load parameters retained so the plugin can be torn down and rebuilt in
    /// place when it requests `kReloadComponent`. See [`Self::reload`].
    reload: ReloadParams,
}

/// Everything `Vst3Instance::load` needs, kept so a `kReloadComponent` request
/// can reconstruct the instance with identical settings.
#[derive(Clone)]
struct ReloadParams {
    path: std::path::PathBuf,
    sample_rate: f64,
    block_size: usize,
    prefer_f64: bool,
}

/// Outcome of [`Vst3Instance::poll_restart`] — the host-side restart effects
/// that could not be applied in place and need the server / client to react.
/// The CC-mapping rebuild and bus re-enumeration are already done by the time
/// this returns; these flags are what remains.
#[derive(Debug, Default, Clone, Copy)]
pub struct RestartChanges {
    /// New latency in samples (re-read because `kLatencyChanged` fired). Push
    /// to PDC.
    pub latency: Option<usize>,
    /// `kParamValuesChanged` — the client should re-read parameter values.
    pub param_values_changed: bool,
    /// `kParamTitlesChanged` — the client should re-pull the parameter list.
    pub param_titles_changed: bool,
    /// `kReloadComponent` — the instance was torn down and rebuilt in place;
    /// the client should resync everything (it is effectively a fresh plugin).
    pub reloaded: bool,
    /// `kIoChanged` — the bus layout changed and was re-enumerated; the new
    /// layout is in `metadata()`. The client should rewire its audio graph.
    pub io_changed: bool,
}

/// Map a `tutti_vst3_host::Vst3Error` to the server's `BridgeError`.
fn map_vst3_error(e: tutti_vst3_host::Vst3Error, path: &Path) -> BridgeError {
    match e {
        tutti_vst3_host::Vst3Error::LoadFailed { path, stage, reason } => {
            BridgeError::LoadFailed { path, stage, reason }
        }
        tutti_vst3_host::Vst3Error::PluginError { stage, code } => {
            BridgeError::PluginError { stage, code }
        }
        _ => BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: e.to_string(),
        },
    }
}

/// Resolve, load, and activate the inner host instance from load parameters,
/// returning it alongside freshly-built protocol metadata. Shared by
/// [`Vst3Instance::load`] and [`Vst3Instance::reload`].
fn build_inner(p: &ReloadParams) -> Result<(VstInner, PluginInfo)> {
    let resolved = tutti_plugin::server::resolve_bundle(&p.path)?;
    let loaded =
        tutti_vst3_host::Vst3Loaded::load(&resolved).map_err(|e| map_vst3_error(e, &p.path))?;

    let info = loaded.info().clone();
    let has_editor = loaded.has_editor();

    let inner = if p.prefer_f64 && info.supports_f64 {
        let inst = loaded
            .activate::<f64>(p.sample_rate, p.block_size)
            .map_err(|e| map_vst3_error(e, &p.path))?;
        VstInner::F64(inst)
    } else {
        let inst = loaded
            .activate::<f32>(p.sample_rate, p.block_size)
            .map_err(|e| map_vst3_error(e, &p.path))?;
        VstInner::F32(inst)
    };

    let actually_f64 = matches!(inner, VstInner::F64(_));
    let latency = match &inner {
        VstInner::F32(i) => i.read_latency_samples(),
        VstInner::F64(i) => i.read_latency_samples(),
    } as usize;
    let buses = build_bus_layout(&info);
    let metadata = PluginInfo::new(info.id.clone(), info.name.clone())
        .author(info.vendor.clone())
        .version(info.version.clone())
        .audio_io(info.num_inputs, info.num_outputs)
        .midi(info.has_midi_input)
        .f64_support(actually_f64)
        .buses(buses)
        .editor(has_editor, None)
        .latency(latency);

    Ok((inner, metadata))
}

impl Vst3Instance {
    /// Lightweight probe: load library and read factory metadata without activation.
    pub fn probe(path: &Path) -> Result<PluginInfo> {
        let resolved = tutti_plugin::server::resolve_bundle(path)?;
        let info = tutti_vst3_host::Vst3Instance::<f32>::probe(&resolved).map_err(|e| {
            BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Scanning,
                reason: e.to_string(),
            }
        })?;
        Ok(PluginInfo::new(info.id.clone(), info.name.clone())
            .author(info.vendor.clone())
            .version(info.version.clone())
            .audio_io(info.num_inputs, info.num_outputs)
            .midi(info.has_midi_input)
            .f64_support(info.supports_f64))
    }

    /// Load and activate a VST3 plugin.
    ///
    /// If `prefer_f64` is `true` and the plugin advertises 64-bit support, the
    /// inner instance is activated as `Vst3Instance<f64>`; otherwise `f32` is
    /// used. The chosen format is reflected in `metadata().supports_f64`.
    pub fn load(path: &Path, sample_rate: f64, block_size: usize, prefer_f64: bool) -> Result<Self> {
        let reload = ReloadParams {
            path: path.to_path_buf(),
            sample_rate,
            block_size,
            prefer_f64,
        };
        let (inner, metadata) = build_inner(&reload)?;
        Ok(Self {
            inner,
            metadata,
            param_cache: ParamCache::default(),
            reload,
        })
    }

    /// Tear the instance down and rebuild it from the original load parameters,
    /// preserving plugin state across the swap. Invoked when the plugin
    /// requests `kReloadComponent` (e.g. after an in-plugin preset load that
    /// changes the component structure). The rebuilt instance replaces `inner`
    /// and `metadata` in place.
    fn reload(&mut self) -> Result<()> {
        // Capture state from the old instance so the rebuilt one resumes where
        // it left off; tolerate plugins that refuse getState.
        let saved_state = vst_dispatch_mut!(self, inner => inner.state()).ok();

        let (inner, metadata) = build_inner(&self.reload)?;
        self.inner = inner;
        self.metadata = metadata;
        self.param_cache = ParamCache::default();

        if let Some(state) = saved_state {
            let _ = vst_dispatch_mut!(self, inner => inner.set_state(&state));
        }
        Ok(())
    }

    pub fn metadata(&self) -> &PluginInfo {
        &self.metadata
    }

    /// Drain the plugin's `restartComponent` requests and apply every host-side
    /// effect, returning the residual [`RestartChanges`] the server / client
    /// still needs to react to.
    ///
    /// Applied in place here:
    /// - `kLatencyChanged` → re-read latency, update cached metadata (returned
    ///   in `latency` for PDC).
    /// - `kMidiCCAssignmentChanged` → re-query the `IMidiMapping` CC→param table
    ///   (`rebuild_midi_cc_mapping`), so runtime CC remaps take effect.
    /// - `kReloadComponent` → tear the instance down and rebuild it from the
    ///   original load params, preserving state (`reloaded`).
    /// - `kIoChanged` → the host re-enumerated buses; refresh cached bus
    ///   metadata and flag `io_changed` so the client can rewire.
    ///
    /// Surfaced for the client (no host-side action possible): the parameter
    /// re-read flags.
    ///
    /// Polled between audio blocks by [`Plugin::poll_async_events`]; it must
    /// not be called concurrently with [`process`](Self::process).
    pub fn poll_restart(&mut self) -> RestartChanges {
        let restart = vst_dispatch_mut!(self, inner => inner.poll_plugin_notifications()).restart;
        let mut changes = RestartChanges::default();

        if restart.latency_changed {
            let samples =
                vst_dispatch_mut!(self, inner => inner.read_latency_samples()) as usize;
            self.metadata = self.metadata.clone().latency(samples);
            changes.latency = Some(samples);
        }

        if restart.midi_cc_assignment_changed {
            vst_dispatch_mut!(self, inner => inner.rebuild_midi_cc_mapping());
        }

        changes.param_values_changed = restart.param_values_changed;
        changes.param_titles_changed = restart.param_titles_changed;

        if restart.io_changed {
            // The host already re-enumerated buses on its side; refresh the
            // cached layout so metadata() reflects it for the client rewire.
            let buses = vst_dispatch!(self, inner => build_bus_layout(inner.info()));
            self.metadata = self.metadata.clone().buses(buses);
            changes.io_changed = true;
        }

        if restart.reload_requested {
            // A failed reload leaves the old instance in place; surface nothing
            // rather than tearing the plugin down on a transient error.
            if self.reload().is_ok() {
                changes.reloaded = true;
                // Reload re-read latency into the fresh metadata; propagate it.
                changes.latency = Some(self.metadata.latency_samples);
            }
        }

        changes
    }

    pub fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        let count = vst_dispatch!(self, inner => inner.parameter_count());
        (0..count)
            .filter_map(|i| {
                vst_dispatch!(self, inner => inner.parameter_info(i)).map(build_param_info)
            })
            .collect()
    }

    pub fn get_parameter_info(&mut self, param_id: u32) -> Option<ParameterInfo> {
        let list = self.get_parameter_list();
        self.param_cache.lookup(param_id, || list)
    }

    pub fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        // Safety: WindowHandle was validated at the IPC boundary in server.rs
        let handle = unsafe { tutti_vst3_host::WindowHandle::from_raw(parent.as_ptr()) };
        vst_dispatch_mut!(self, inner => inner.open_editor(handle))
            .map(|size| EditorSize { width: size.width, height: size.height })
            .map_err(|e| BridgeError::EditorError(e.to_string()))
    }
}

/// Process one audio block through a typed `Vst3Instance<T>`.
fn process_block<'a, T: tutti_vst3_host::Vst3Sample>(
    inner: &mut tutti_vst3_host::Vst3Instance<T>,
    inputs: &'a [&'a [T]],
    outputs: &'a mut [&'a mut [T]],
    sample_rate: f64,
    ctx: &tutti_plugin::server::ProcessContext,
) -> Result<tutti_plugin::server::ProcessOutput> {
    let mut vst3_buffer = tutti_vst3_host::AudioBuffer::new(inputs, outputs, sample_rate);
    let vst3_transport = ctx.transport.cloned().unwrap_or_default();
    let vst3_note_expr = ctx
        .note_expression
        .map(convert_note_expression_to_vst3)
        .unwrap_or_default();
    let vst3_chords = ctx.chords.map(convert_chords_to_vst3).unwrap_or_default();
    let vst3_scales = ctx.scales.map(convert_scales_to_vst3).unwrap_or_default();
    let vst3_expr_texts = ctx
        .expr_texts
        .map(convert_expr_texts_to_vst3)
        .unwrap_or_default();
    let vst3_expr_ints = ctx
        .expr_ints
        .map(convert_expr_ints_to_vst3)
        .unwrap_or_default();
    let output = inner.process(
        &mut vst3_buffer,
        ctx.midi_events,
        ctx.param_changes,
        &vst3_note_expr,
        &vst3_chords,
        &vst3_scales,
        &vst3_expr_texts,
        &vst3_expr_ints,
        &vst3_transport,
    );
    let midi_events = output.midi_events.iter().copied().collect();
    let param_changes = output.parameter_changes.clone();
    Ok(tutti_plugin::server::ProcessOutput {
        midi_events,
        param_changes,
        note_expression: NoteExpressionChanges::new(),
    })
}

/// Translate the vst3-host's per-bus channel enumeration into protocol
/// [`BusLayout`]s (input buses then output buses, each in bus-index order).
///
/// Returns an empty list for single-bus plugins (≤1 bus per direction) so the
/// wire format is byte-identical to today and the server's single-bus path is
/// unchanged. A non-empty list is emitted only when a plugin actually exposes
/// an aux/sidechain bus.
fn build_bus_layout(info: &tutti_vst3_host::PluginInfo) -> Vec<BusLayout> {
    let multi_input = info.input_bus_channels.len() > 1;
    let multi_output = info.output_bus_channels.len() > 1;
    if !multi_input && !multi_output {
        return Vec::new();
    }
    let mut buses =
        Vec::with_capacity(info.input_bus_channels.len() + info.output_bus_channels.len());
    buses.extend(info.input_bus_channels.iter().map(|&ch| BusLayout::input(ch)));
    buses.extend(info.output_bus_channels.iter().map(|&ch| BusLayout::output(ch)));
    buses
}

/// Build a Tutti [`ParameterInfo`] from a VST3 parameter descriptor. Both the
/// `get_parameter_list` and the cache-warming paths go through here so they
/// agree on the flag mapping and on VST3's normalized 0..1 range convention.
fn build_param_info(info: tutti_vst3_host::Vst3ParameterInfo) -> ParameterInfo {
    let flags = ParameterFlags {
        automatable: info.can_automate(),
        read_only: info.is_read_only(),
        wrap: info.is_wrap(),
        is_bypass: info.is_bypass(),
        hidden: info.is_hidden(),
    };
    make_param_info(
        info.id,
        info.title_string(),
        info.units_string(),
        0.0, // VST3 uses normalized 0-1
        1.0,
        info.default_normalized_value,
        info.step_count as u32,
        flags,
    )
}

fn convert_note_expression_to_vst3(
    note_expr: &NoteExpressionChanges,
) -> Vec<tutti_vst3_host::NoteExpressionValue> {
    note_expr
        .changes
        .iter()
        .map(|e| tutti_vst3_host::NoteExpressionValue {
            sample_offset: e.sample_offset,
            note_id: e.note_id,
            expression_type: match e.expression_type {
                NoteExpressionType::Volume => tutti_vst3_host::NoteExpressionType::Volume,
                NoteExpressionType::Pan => tutti_vst3_host::NoteExpressionType::Pan,
                NoteExpressionType::Tuning => tutti_vst3_host::NoteExpressionType::Tuning,
                NoteExpressionType::Vibrato => tutti_vst3_host::NoteExpressionType::Vibrato,
                NoteExpressionType::Brightness => tutti_vst3_host::NoteExpressionType::Brightness,
            },
            value: e.value,
        })
        .collect()
}

fn convert_chords_to_vst3(chords: &ChordChanges) -> Vec<tutti_vst3_host::ChordValue> {
    chords
        .changes
        .iter()
        .map(|c| tutti_vst3_host::ChordValue {
            sample_offset: c.sample_offset,
            root: c.root,
            bass_note: c.bass_note,
            mask: c.mask,
            text: c.text.encode_utf16().collect(),
        })
        .collect()
}

fn convert_scales_to_vst3(scales: &ScaleChanges) -> Vec<tutti_vst3_host::ScaleValue> {
    scales
        .changes
        .iter()
        .map(|s| tutti_vst3_host::ScaleValue {
            sample_offset: s.sample_offset,
            root: s.root,
            mask: s.mask,
            text: s.text.encode_utf16().collect(),
        })
        .collect()
}

fn convert_expr_texts_to_vst3(
    texts: &NoteExpressionTextChanges,
) -> Vec<tutti_vst3_host::NoteExpressionText> {
    texts
        .changes
        .iter()
        .map(|t| tutti_vst3_host::NoteExpressionText {
            sample_offset: t.sample_offset,
            note_id: t.note_id,
            type_id: t.type_id,
            text: t.text.encode_utf16().collect(),
        })
        .collect()
}

fn convert_expr_ints_to_vst3(
    ints: &NoteExpressionIntChanges,
) -> Vec<tutti_vst3_host::NoteExpressionIntValue> {
    ints.changes
        .iter()
        .map(|i| tutti_vst3_host::NoteExpressionIntValue {
            sample_offset: i.sample_offset,
            note_id: i.note_id,
            type_id: i.type_id,
            value: i.value,
        })
        .collect()
}

impl tutti_plugin::server::PluginInstance for Vst3Instance {
    fn metadata(&self) -> &PluginInfo {
        &self.metadata
    }

    fn process(
        &mut self,
        buffer: tutti_plugin::server::AudioBufferMut<'_>,
        ctx: &tutti_plugin::server::ProcessContext,
    ) -> Result<tutti_plugin::server::ProcessOutput> {
        use tutti_plugin::server::AudioBufferMut;
        match (&mut self.inner, buffer) {
            (VstInner::F32(inner), AudioBufferMut::F32(buf)) => {
                process_block(inner, buf.inputs, buf.outputs, buf.sample_rate, ctx)
            }
            (VstInner::F64(inner), AudioBufferMut::F64(buf)) => {
                process_block(inner, buf.inputs, buf.outputs, buf.sample_rate, ctx)
            }
            _ => Err(BridgeError::LoadFailed {
                path: std::path::PathBuf::new(),
                stage: LoadStage::Initialization,
                reason: "Buffer format mismatch: plugin was activated with a different sample format".to_string(),
            }),
        }
    }

    fn set_sample_rate(&mut self, rate: f64) {
        vst_dispatch_mut!(self, inner => {
            inner.set_sample_rate(rate);
        });
    }

    fn get_parameter(&self, id: u32) -> f64 {
        vst_dispatch!(self, inner => inner.parameter(id))
    }

    fn set_parameter(&mut self, id: u32, value: f64) {
        vst_dispatch_mut!(self, inner => inner.set_parameter(id, value));
    }

    fn get_parameter_list(&mut self) -> Vec<tutti_plugin::server::ParameterInfo> {
        Vst3Instance::get_parameter_list(self)
    }

    fn get_parameter_info(&mut self, id: u32) -> Option<tutti_plugin::server::ParameterInfo> {
        Vst3Instance::get_parameter_info(self, id)
    }

    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        Vst3Instance::open_editor(self, parent)
    }

    fn close_editor(&mut self) {
        vst_dispatch_mut!(self, inner => inner.close_editor());
    }

    fn editor_idle(&mut self) {
        // VST3 doesn't have explicit idle
    }

    fn get_state(&mut self) -> Result<Vec<u8>> {
        vst_dispatch_mut!(self, inner => inner.state())
            .map_err(|e| BridgeError::StateSaveError(e.to_string()))
    }

    fn set_state(&mut self, data: &[u8]) -> Result<()> {
        vst_dispatch_mut!(self, inner => inner.set_state(data))
            .map_err(|e| BridgeError::StateRestoreError(e.to_string()))
    }
}

#[cfg(test)]
#[cfg(feature = "vst3")]
mod tests {
    use super::*;
    use std::path::Path;
    use tutti_plugin::server::{
        AudioBuffer, AudioBuffer64, AudioBufferMut, MidiEvent, PluginInstance,
    };

    const VST3_PLUGIN: &str = "/Library/Audio/Plug-Ins/VST3/TAL-NoiseMaker.vst3";

    #[test]
    fn test_vst3_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let instance = Vst3Instance::load(path, 44100.0, 512, false);
        assert!(
            instance.is_ok(),
            "Failed to load VST3 plugin: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        let meta = instance.metadata();
        assert!(!meta.name.is_empty(), "Plugin name should not be empty");
        assert!(!meta.id.is_empty(), "Plugin id should not be empty");
    }

    #[test]
    fn test_vst3_metadata() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let instance = Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");
        let meta = instance.metadata();

        assert!(
            meta.audio_io.outputs > 0,
            "Expected audio outputs > 0, got {}",
            meta.audio_io.outputs
        );
    }

    #[test]
    fn test_vst3_parameter_count() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let instance = Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");

        let count = instance.get_parameter_list().len();
        assert!(
            count > 0,
            "TAL-NoiseMaker should have parameters, got {}",
            count
        );
    }

    #[test]
    fn test_vst3_parameter_list() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let instance = Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");

        let params = instance.get_parameter_list();
        assert!(
            !params.is_empty(),
            "Parameter list should not be empty for TAL-NoiseMaker"
        );

        for param in &params {
            assert!(
                !param.name.is_empty(),
                "Parameter id {} has empty name",
                param.id
            );
        }
    }

    #[test]
    fn test_vst3_get_parameter() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let instance = Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "Need at least one parameter");

        let first_id = params[0].id;
        let value = PluginInstance::get_parameter(&instance, first_id);
        assert!(
            value.is_finite(),
            "Parameter value should be finite, got {}",
            value
        );
    }

    #[test]
    fn test_vst3_process_f32_silence() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let mut instance =
            Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = tutti_plugin::server::ProcessContext::new();
        // Should not panic
        let _output = instance.process(AudioBufferMut::F32(buffer), &ctx);
    }

    #[test]
    fn test_vst3_process_f32_with_note() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let mut instance =
            Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");

        let num_samples = 512;
        let note_on = [MidiEvent::note_on(
            0,
            1,
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        )];

        let mut has_nonzero = false;

        // Process block with NoteOn
        {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let ctx = tutti_plugin::server::ProcessContext::new().midi(&note_on);
            let _output = instance.process(AudioBufferMut::F32(buffer), &ctx);

            for ch in output_data.iter() {
                for &sample in ch.iter() {
                    if sample != 0.0 {
                        has_nonzero = true;
                    }
                }
            }
        }

        // Process additional blocks to give the synth time to produce sound
        let empty_ctx = tutti_plugin::server::ProcessContext::new();
        for _ in 0..4 {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let _output = instance.process(AudioBufferMut::F32(buffer), &empty_ctx);

            for ch in output_data.iter() {
                for &sample in ch.iter() {
                    if sample != 0.0 {
                        has_nonzero = true;
                    }
                }
            }
        }

        assert!(
            has_nonzero,
            "Expected at least one non-zero output sample after NoteOn"
        );
    }

    // =========================================================================
    // Voxengo SPAN / Boogex — f64 support tests (local fixture plugins)
    // =========================================================================

    const SPAN_VST3: &str = "tests/fixtures/plugins/SPAN.vst3";
    const BOOGEX_VST3: &str = "tests/fixtures/plugins/Boogex.vst3";

    /// Resolve fixture path relative to workspace root.
    fn fixture_path(relative: &str) -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // Go up from crates/tutti-plugin-server to the workspace root (crates/tutti)
        p.pop();
        p.pop();
        p.push(relative);
        p
    }

    #[test]
    fn test_voxengo_span_load_and_f64_support() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(SPAN_VST3);
        if !path.exists() {
            eprintln!("Skipping: SPAN.vst3 not found at {:?}", path);
            return;
        }
        let instance = Vst3Instance::load(&path, 44100.0, 512, false);
        assert!(
            instance.is_ok(),
            "Failed to load SPAN: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        let meta = instance.metadata();
        eprintln!(
            "SPAN: name={}, supports_f64={}",
            meta.name, meta.supports_f64
        );
        assert!(!meta.name.is_empty());
    }

    #[test]
    fn test_voxengo_boogex_load_and_f64_support() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(BOOGEX_VST3);
        if !path.exists() {
            eprintln!("Skipping: Boogex.vst3 not found at {:?}", path);
            return;
        }
        let instance = Vst3Instance::load(&path, 44100.0, 512, false);
        assert!(
            instance.is_ok(),
            "Failed to load Boogex: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        let meta = instance.metadata();
        eprintln!(
            "Boogex: name={}, supports_f64={}",
            meta.name, meta.supports_f64
        );
        assert!(!meta.name.is_empty());
    }

    #[test]
    fn test_voxengo_span_process_f64() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(SPAN_VST3);
        if !path.exists() {
            eprintln!("Skipping: SPAN.vst3 not found at {:?}", path);
            return;
        }
        // Load with f64 preference — if the plugin supports it, inner will be F64.
        let mut instance = Vst3Instance::load(&path, 44100.0, 512, true).expect("Failed to load SPAN");

        if !instance.metadata().supports_f64 {
            eprintln!("SPAN does not report f64 support, skipping f64 test");
            return;
        }

        let num_samples = 512;
        // Feed a 440Hz sine wave to test pass-through
        let input_data: Vec<Vec<f64>> = (0..2)
            .map(|_| {
                (0..num_samples)
                    .map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() * 0.5)
                    .collect()
            })
            .collect();
        let mut output_data = vec![vec![0.0f64; num_samples]; 2];

        let input_slices: Vec<&[f64]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f64]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = AudioBuffer64 {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = tutti_plugin::server::ProcessContext::new();
        let _output = instance.process(AudioBufferMut::F64(buffer), &ctx);

        // SPAN is an analyzer — it should pass audio through unchanged
        let mut all_zero = true;
        for ch in &output_data {
            for &s in ch {
                if s != 0.0 {
                    all_zero = false;
                    break;
                }
            }
        }
        eprintln!(
            "SPAN f64 output: first sample = {}, all_zero = {}",
            output_data[0][0], all_zero
        );
    }

    #[test]
    fn test_voxengo_boogex_process_f64() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(BOOGEX_VST3);
        if !path.exists() {
            eprintln!("Skipping: Boogex.vst3 not found at {:?}", path);
            return;
        }
        // Load with f64 preference — if the plugin supports it, inner will be F64.
        let mut instance = Vst3Instance::load(&path, 44100.0, 512, true).expect("Failed to load Boogex");

        if !instance.metadata().supports_f64 {
            eprintln!("Boogex does not report f64 support, skipping f64 test");
            return;
        }

        let num_samples = 512;
        // Feed a sine wave — Boogex is an amp sim so it should transform the audio
        let input_data: Vec<Vec<f64>> = (0..2)
            .map(|_| {
                (0..num_samples)
                    .map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() * 0.5)
                    .collect()
            })
            .collect();
        let mut output_data = vec![vec![0.0f64; num_samples]; 2];

        let input_slices: Vec<&[f64]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f64]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = AudioBuffer64 {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = tutti_plugin::server::ProcessContext::new();
        let _output = instance.process(AudioBufferMut::F64(buffer), &ctx);

        let mut has_nonzero = false;
        for ch in &output_data {
            for &s in ch {
                if s != 0.0 {
                    has_nonzero = true;
                    break;
                }
            }
        }
        eprintln!(
            "Boogex f64 output: first sample = {}, has_nonzero = {}",
            output_data[0][0], has_nonzero
        );
    }
}
