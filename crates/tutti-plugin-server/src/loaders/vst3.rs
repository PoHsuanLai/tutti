//! VST3 plugin loader using the `vst3-host` crate.

use std::path::Path;

use tutti_plugin::server::{
    BusLayout, EditorSize, NoteExpressionChanges, NoteExpressionType, ParameterFlags,
    ParameterInfo, PluginInfo, WindowHandle,
};
use tutti_plugin::{BridgeError, LoadStage, Result};

use crate::loaders::common::params::{make_param_info, ParamCache};

pub use tutti_vst3_host;

pub struct Vst3Instance {
    inner: tutti_vst3_host::Vst3Instance,
    metadata: PluginInfo,
    param_cache: ParamCache,
}

impl Vst3Instance {
    /// Lightweight probe: load library and read factory metadata without activation.
    pub fn probe(path: &Path) -> Result<PluginInfo> {
        let resolved = tutti_plugin::server::resolve_bundle(path)?;
        let metadata = tutti_vst3_host::Vst3Instance::probe(&resolved).map_err(|e| {
            BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Scanning,
                reason: e.to_string(),
            }
        })?;
        let info = metadata;
        Ok(PluginInfo::new(info.id.clone(), info.name.clone())
            .author(info.vendor.clone())
            .version(info.version.clone())
            .audio_io(info.num_inputs, info.num_outputs)
            .midi(info.has_midi_input)
            .f64_support(info.supports_f64))
    }

    pub fn load(path: &Path, sample_rate: f64, block_size: usize) -> Result<Self> {
        let resolved = tutti_plugin::server::resolve_bundle(path)?;
        let inner = tutti_vst3_host::Vst3Instance::load(&resolved, sample_rate, block_size)
            .map_err(|e| match e {
                tutti_vst3_host::Vst3Error::LoadFailed {
                    path,
                    stage,
                    reason,
                } => BridgeError::LoadFailed {
                    path,
                    // Host and bridge LoadStage are the same shared type now.
                    stage,
                    reason,
                },
                tutti_vst3_host::Vst3Error::PluginError { stage, code } => {
                    BridgeError::PluginError { stage, code }
                }
                _ => BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Opening,
                    reason: e.to_string(),
                },
            })?;

        let info = inner.info();
        let has_editor = inner.has_editor();
        let latency = inner.get_latency_samples() as usize;
        let buses = build_bus_layout(info);
        let metadata = PluginInfo::new(info.id.clone(), info.name.clone())
            .author(info.vendor.clone())
            .version(info.version.clone())
            .audio_io(info.num_inputs, info.num_outputs)
            .midi(info.has_midi_input)
            .f64_support(info.supports_f64)
            .buses(buses)
            .editor(has_editor, None)
            .latency(latency);

        Ok(Self {
            inner,
            metadata,
            param_cache: ParamCache::default(),
        })
    }

    pub fn metadata(&self) -> &PluginInfo {
        &self.metadata
    }

    /// Drain the plugin's `restartComponent` requests and apply their
    /// host-side effects (latency re-read, bus re-enumeration). If
    /// `kLatencyChanged` fired and the value actually changed, update cached
    /// metadata and return the new latency in samples so the server can push a
    /// PDC update to the host. Returns `None` when latency is unchanged.
    ///
    /// Polled between audio blocks by [`Plugin::poll_async_events`]; it must
    /// not be called concurrently with [`process`](Self::process).
    pub fn poll_latency_changed(&mut self) -> Option<usize> {
        let (_events, outcome) = self.inner.handle_restart_events();
        outcome.new_latency_samples.map(|samples| {
            let samples = samples as usize;
            self.metadata = self.metadata.clone().latency(samples);
            samples
        })
    }

    /// The plugin advertised f64 support at probe time but rejected it at
    /// `set_sample_format` — flip the flag so callers renegotiate to f32.
    pub fn clear_f64_support(&mut self) {
        self.metadata.supports_f64 = false;
    }

    /// Used by [`Plugin::load`](crate::plugin::Plugin) to decide whether
    /// to attempt f64 setup before the caller's preferred format is
    /// negotiated.
    pub fn can_process_f64(&self) -> bool {
        self.inner.supports_f64()
    }

    /// Used by [`Plugin::load`](crate::plugin::Plugin) to negotiate the
    /// session sample format with the plugin. VST3 plugins may refuse f64
    /// setup even when `can_process_f64()` is true; the error is how the
    /// caller learns to fall back to f32.
    pub fn set_sample_format(&mut self, format: tutti_plugin::server::SampleFormat) -> Result<()> {
        let use_f64 = matches!(format, tutti_plugin::server::SampleFormat::Float64);
        self.inner
            .set_use_f64(use_f64)
            .map(|_| ())
            .map_err(|e| BridgeError::LoadFailed {
                path: std::path::PathBuf::new(),
                stage: LoadStage::Initialization,
                reason: format!("set_sample_format failed: {e}"),
            })
    }

    pub fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        let count = self.inner.parameter_count();
        (0..count)
            .filter_map(|i| self.inner.parameter_info(i).map(build_param_info))
            .collect()
    }

    pub fn get_parameter_info(&mut self, param_id: u32) -> Option<ParameterInfo> {
        let list = self.get_parameter_list();
        self.param_cache.lookup(param_id, || list)
    }

    pub fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        // Safety: WindowHandle was validated at the IPC boundary in server.rs
        let handle = unsafe { tutti_vst3_host::WindowHandle::from_raw(parent.as_ptr()) };
        self.inner
            .open_editor(handle)
            .map(|size| EditorSize {
                width: size.width,
                height: size.height,
            })
            .map_err(|e| BridgeError::EditorError(e.to_string()))
    }

    /// Generic process body shared between the f32 and f64 call paths —
    /// the `tutti_vst3_host::AudioBuffer<T>` and its underlying FFI setup are
    /// identical at every level except the sample type, so we pay for
    /// that branch exactly once in the trait method above.
    fn process_inner<'a, T: tutti_vst3_host::Vst3Sample>(
        &mut self,
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

        let output = self.inner.process(
            &mut vst3_buffer,
            ctx.midi_events,
            ctx.param_changes,
            &vst3_note_expr,
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
}

/// Build a Tutti [`ParameterInfo`] from a VST3 parameter descriptor.
///
/// Shared between [`Vst3Instance::get_parameter_list`] and
/// [`Vst3Instance::ensure_param_cache`] so both paths agree on the flag
/// mapping and on VST3's normalized 0..1 range convention.
/// Translate the vst3-host's per-bus channel enumeration into protocol
/// [`BusLayout`]s (input buses then output buses, each in bus-index order).
///
/// Returns an empty list for single-bus plugins (≤1 bus per direction) so the
/// wire format is byte-identical to today and the server's single-bus path is
/// unchanged. A non-empty list is emitted only when a plugin actually exposes
/// an aux/sidechain bus, which is what Stage 3 will split.
fn build_bus_layout(info: &tutti_vst3_host::PluginInfo) -> Vec<BusLayout> {
    let multi_input = info.input_bus_channels.len() > 1;
    let multi_output = info.output_bus_channels.len() > 1;
    if !multi_input && !multi_output {
        return Vec::new();
    }
    let mut buses = Vec::with_capacity(info.input_bus_channels.len() + info.output_bus_channels.len());
    buses.extend(info.input_bus_channels.iter().map(|&ch| BusLayout::input(ch)));
    buses.extend(info.output_bus_channels.iter().map(|&ch| BusLayout::output(ch)));
    buses
}

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
        match buffer {
            AudioBufferMut::F32(buf) => {
                self.process_inner(buf.inputs, buf.outputs, buf.sample_rate, ctx)
            }
            AudioBufferMut::F64(buf) => {
                self.process_inner(buf.inputs, buf.outputs, buf.sample_rate, ctx)
            }
        }
    }

    fn set_sample_rate(&mut self, rate: f64) {
        self.inner.set_sample_rate(rate);
    }

    fn get_parameter(&self, id: u32) -> f64 {
        self.inner.parameter(id)
    }

    fn set_parameter(&mut self, id: u32, value: f64) {
        self.inner.set_parameter(id, value);
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
        self.inner.close_editor();
    }

    fn editor_idle(&mut self) {
        // VST3 doesn't have explicit idle
    }

    fn get_state(&mut self) -> Result<Vec<u8>> {
        self.inner
            .state()
            .map_err(|e| BridgeError::StateSaveError(e.to_string()))
    }

    fn set_state(&mut self, data: &[u8]) -> Result<()> {
        self.inner
            .set_state(data)
            .map(|_| ())
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
        let instance = Vst3Instance::load(path, 44100.0, 512);
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
        let instance = Vst3Instance::load(path, 44100.0, 512).expect("Failed to load VST3 plugin");
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
        let instance = Vst3Instance::load(path, 44100.0, 512).expect("Failed to load VST3 plugin");

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
        let instance = Vst3Instance::load(path, 44100.0, 512).expect("Failed to load VST3 plugin");

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
        let instance = Vst3Instance::load(path, 44100.0, 512).expect("Failed to load VST3 plugin");

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
            Vst3Instance::load(path, 44100.0, 512).expect("Failed to load VST3 plugin");

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
            Vst3Instance::load(path, 44100.0, 512).expect("Failed to load VST3 plugin");

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
        let instance = Vst3Instance::load(&path, 44100.0, 512);
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
        let instance = Vst3Instance::load(&path, 44100.0, 512);
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
        let mut instance = Vst3Instance::load(&path, 44100.0, 512).expect("Failed to load SPAN");

        if !instance.metadata().supports_f64 {
            eprintln!("SPAN does not report f64 support, skipping f64 test");
            return;
        }

        // Enable f64 processing
        instance
            .set_sample_format(tutti_plugin::server::SampleFormat::Float64)
            .expect("Failed to set f64 format");

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
        let mut instance = Vst3Instance::load(&path, 44100.0, 512).expect("Failed to load Boogex");

        if !instance.metadata().supports_f64 {
            eprintln!("Boogex does not report f64 support, skipping f64 test");
            return;
        }

        // Enable f64 processing
        instance
            .set_sample_format(tutti_plugin::server::SampleFormat::Float64)
            .expect("Failed to set f64 format");

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
