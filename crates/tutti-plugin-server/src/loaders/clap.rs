//! CLAP plugin loader - thin wrapper around clap-host crate.

use std::path::Path;
use tutti_plugin::server::{
    EditorSize, NoteExpressionChanges, ParameterChanges, ParameterFlags, ParameterInfo, PluginInfo,
    WindowHandle,
};
use tutti_plugin::server::{PluginInstance, ProcessContext, ProcessOutput};

use crate::loaders::common::params::make_param_info;
use tutti_plugin::{BridgeError, LoadStage, Result};

#[cfg(feature = "clap")]
use tutti_clap_host::{ClapActive, ClapLoaded};

/// Active host instance, monomorphised by sample format at activation time.
/// CLAP advertises f32 in metadata today, so the F32 arm is the live path; the
/// F64 arm is wired for symmetry and future 64-bit support.
#[cfg(feature = "clap")]
enum ClapInner {
    F32(ClapActive<f32>),
    F64(ClapActive<f64>),
}

/// Dispatch a shared expression over both inner variants (immutable). The bound
/// methods come from `ClapLoaded` via `Deref`, so they resolve on either arm.
#[cfg(feature = "clap")]
macro_rules! clap_dispatch {
    ($self:expr, $inner:ident => $body:expr) => {
        match &$self.inner {
            ClapInner::F32($inner) => $body,
            ClapInner::F64($inner) => $body,
        }
    };
}

/// Dispatch a shared expression over both inner variants (mutable).
#[cfg(feature = "clap")]
macro_rules! clap_dispatch_mut {
    ($self:expr, $inner:ident => $body:expr) => {
        match &mut $self.inner {
            ClapInner::F32($inner) => $body,
            ClapInner::F64($inner) => $body,
        }
    };
}

pub struct ClapInstance {
    #[cfg(feature = "clap")]
    inner: ClapInner,
    metadata: PluginInfo,
}

// Safety: the inner host instances are Send.
unsafe impl Send for ClapInstance {}

impl ClapInstance {
    /// Lightweight probe: read CLAP descriptor without calling init() or activate().
    pub fn probe(path: &Path) -> Result<PluginInfo> {
        #[cfg(feature = "clap")]
        {
            let resolved = tutti_plugin::server::resolve_bundle(path)?;
            let library_path = if resolved != path {
                Some(resolved.as_path())
            } else {
                None
            };
            let info = ClapLoaded::probe(path, library_path).map_err(|e| {
                BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Scanning,
                    reason: e.to_string(),
                }
            })?;

            let receives_midi = info
                .features
                .iter()
                .any(|f| f == "instrument" || f == "synthesizer" || f == "note-effect");

            Ok(PluginInfo::new(info.id.clone(), info.name.clone())
                .author(info.vendor.clone())
                .version(info.version.clone())
                .audio_io(info.audio_inputs, info.audio_outputs)
                .midi(receives_midi)
                .f64_support(false))
        }
        #[cfg(not(feature = "clap"))]
        Err(BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: "CLAP support not compiled".to_string(),
        })
    }

    pub fn load(path: &Path, sample_rate: f64, block_size: usize) -> Result<Self> {
        #[cfg(feature = "clap")]
        {
            let resolved = tutti_plugin::server::resolve_bundle(path)?;
            let library_path = if resolved != path {
                Some(resolved.as_path())
            } else {
                None
            };
            let loaded = ClapLoaded::load_with_library(
                path,
                library_path,
                sample_rate,
                block_size as u32,
            )
            .map_err(|e| BridgeError::LoadFailed {
                path: path.to_path_buf(),
                // Host and bridge LoadStage are now the same shared type
                // (tutti-plugin-types); pass the stage through unchanged.
                stage: match e {
                    tutti_clap_host::ClapError::LoadFailed { stage, .. } => stage,
                    _ => LoadStage::Opening,
                },
                reason: e.to_string(),
            })?;

            // Read metadata off the loaded (pre-activation) instance.
            let info = loaded.info();
            let has_note_input = loaded.note_port_count(true) > 0;
            let supports_f64 = loaded.supports_f64();
            let mut metadata = PluginInfo::new(info.id.clone(), info.name.clone())
                .author(info.vendor.clone())
                .version(info.version.clone())
                .audio_io(info.audio_inputs, info.audio_outputs)
                .midi(has_note_input)
                .f64_support(supports_f64)
                .editor(loaded.has_editor(), None);

            // Activate into the typed inner. CLAP advertises f32 today, so the
            // F32 arm is taken; F64 is wired for symmetry.
            let inner = if supports_f64 {
                ClapInner::F64(loaded.activate::<f64>().map_err(|(_, e)| {
                    BridgeError::LoadFailed {
                        path: path.to_path_buf(),
                        stage: LoadStage::Activation,
                        reason: format!("activate failed: {e}"),
                    }
                })?)
            } else {
                ClapInner::F32(loaded.activate::<f32>().map_err(|(_, e)| {
                    BridgeError::LoadFailed {
                        path: path.to_path_buf(),
                        stage: LoadStage::Activation,
                        reason: format!("activate failed: {e}"),
                    }
                })?)
            };

            // Latency is queryable after activation.
            metadata.latency_samples = match &inner {
                ClapInner::F32(i) => i.get_latency(),
                ClapInner::F64(i) => i.get_latency(),
            } as usize;

            Ok(Self { inner, metadata })
        }

        #[cfg(not(feature = "clap"))]
        {
            let _ = (path, sample_rate, block_size);
            Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: "CLAP support not compiled (enable 'clap' feature)".to_string(),
            })
        }
    }

    /// Returns true if the plugin requested a latency change since the last poll.
    /// Clears the flag.
    #[cfg(feature = "clap")]
    pub fn poll_latency_changed(&mut self) -> bool {
        clap_dispatch_mut!(self, i => i.poll_latency_changed())
    }

    /// Current latency in samples, as reported by the plugin.
    #[cfg(feature = "clap")]
    pub fn get_latency(&self) -> u32 {
        clap_dispatch!(self, i => i.get_latency())
    }

    /// Test-only access to the underlying loaded instance (via `Deref` through
    /// whichever active arm). Lets the integration tests exercise the read-only
    /// `ClapLoaded` surface (poll_*, port/note queries, state context, …)
    /// without threading the f32/f64 enum through every assertion.
    #[cfg(all(feature = "clap", test))]
    fn loaded(&self) -> &tutti_clap_host::ClapLoaded {
        clap_dispatch!(self, i => &**i)
    }

    /// Test-only mutable access (for `poll_timers`, `on_main_thread`, …).
    #[cfg(all(feature = "clap", test))]
    fn loaded_mut(&mut self) -> &mut tutti_clap_host::ClapLoaded {
        clap_dispatch_mut!(self, i => &mut **i)
    }

    /// Test-only: has the active instance started processing? (`is_processing`
    /// lives on `ClapActive`, so it is reached through the enum, not `Deref`.)
    #[cfg(all(feature = "clap", test))]
    fn is_processing(&self) -> bool {
        clap_dispatch!(self, i => i.is_processing())
    }

    /// Drive one block through a typed active instance. Free of `self.inner`
    /// so it works for whichever arm of [`ClapInner`] matches the buffer format.
    #[cfg(feature = "clap")]
    fn process_active<'a, T: tutti_clap_host::ClapSample>(
        active: &'a mut ClapActive<T>,
        clap_buffer: &mut tutti_clap_host::AudioBuffer<T>,
        ctx: &ProcessContext,
        sample_rate: f64,
    ) -> Result<tutti_clap_host::instance::ProcessOutputRef<'a>> {
        let param_changes = ctx.param_changes.cloned().unwrap_or_default();
        let note_expressions: Vec<tutti_clap_host::NoteExpressionValue> = ctx
            .note_expression
            .map(convert_note_expressions)
            .unwrap_or_default();
        let transport = ctx.transport.map(|t| convert_transport(t, sample_rate));

        let clap_ctx = tutti_clap_host::ProcessContext {
            midi: ctx.midi_events,
            params: if param_changes.is_empty() {
                None
            } else {
                Some(&param_changes)
            },
            expressions: &note_expressions,
            transport: transport.as_ref(),
        };

        active
            .process(clap_buffer, &clap_ctx)
            .map_err(|e| BridgeError::ProcessError(e.to_string()))
    }
}

#[cfg(feature = "clap")]
impl PluginInstance for ClapInstance {
    fn metadata(&self) -> &PluginInfo {
        &self.metadata
    }

    fn process(
        &mut self,
        buffer: tutti_plugin::server::AudioBufferMut<'_>,
        ctx: &ProcessContext,
    ) -> Result<ProcessOutput> {
        use tutti_plugin::server::AudioBufferMut;
        // The buffer format must match the format the instance was activated
        // with (the `ClapInner` arm). A mismatch is a negotiation bug upstream;
        // surface it rather than silently mis-cast.
        match (buffer, &mut self.inner) {
            (AudioBufferMut::F32(buf), ClapInner::F32(active)) => {
                let sr = buf.sample_rate;
                let mut clap_buffer = tutti_clap_host::AudioBuffer32 {
                    inputs: buf.inputs,
                    outputs: buf.outputs,
                    num_samples: buf.num_samples,
                    sample_rate: sr,
                };
                Ok(convert_process_output(Self::process_active(
                    active,
                    &mut clap_buffer,
                    ctx,
                    sr,
                )?))
            }
            (AudioBufferMut::F64(buf), ClapInner::F64(active)) => {
                let sr = buf.sample_rate;
                let mut clap_buffer = tutti_clap_host::AudioBuffer64 {
                    inputs: buf.inputs,
                    outputs: buf.outputs,
                    num_samples: buf.num_samples,
                    sample_rate: sr,
                };
                Ok(convert_process_output(Self::process_active(
                    active,
                    &mut clap_buffer,
                    ctx,
                    sr,
                )?))
            }
            (AudioBufferMut::F32(_), ClapInner::F64(_))
            | (AudioBufferMut::F64(_), ClapInner::F32(_)) => Err(BridgeError::ProcessError(
                "audio buffer sample format does not match the format the CLAP \
                 plugin was activated with"
                    .to_string(),
            )),
        }
    }

    fn set_sample_rate(&mut self, rate: f64) {
        clap_dispatch_mut!(self, i => {
            i.set_sample_rate(rate);
        });
    }

    fn get_parameter(&self, id: u32) -> f64 {
        clap_dispatch!(self, i => i.parameter(id)).unwrap_or(0.0)
    }

    fn set_parameter(&mut self, id: u32, value: f64) {
        clap_dispatch_mut!(self, i => {
            i.set_parameter(id, value);
        });
    }

    fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        clap_dispatch!(self, i => i.parameters())
            .into_iter()
            .map(convert_param_info)
            .collect()
    }

    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        // Safety: WindowHandle was validated at the IPC boundary in server.rs
        let handle = unsafe { tutti_clap_host::WindowHandle::from_raw(parent.as_ptr()) };
        clap_dispatch_mut!(self, i => i.open_editor(handle))
            .map(|s| EditorSize {
                width: s.width,
                height: s.height,
            })
            .map_err(|e| BridgeError::EditorError(e.to_string()))
    }

    fn close_editor(&mut self) {
        clap_dispatch_mut!(self, i => {
            i.close_editor();
        });
    }

    fn get_state(&mut self) -> Result<Vec<u8>> {
        clap_dispatch_mut!(self, i => i.state())
            .map_err(|e| BridgeError::StateSaveError(e.to_string()))
    }

    fn set_state(&mut self, data: &[u8]) -> Result<()> {
        clap_dispatch_mut!(self, i => i.set_state(data))
            .map_err(|e| BridgeError::StateRestoreError(e.to_string()))
    }
}

#[cfg(feature = "clap")]
fn convert_note_expressions(
    changes: &NoteExpressionChanges,
) -> Vec<tutti_clap_host::NoteExpressionValue> {
    changes
        .changes
        .iter()
        .map(|expr| {
            let expression_type = match expr.expression_type {
                tutti_plugin::server::NoteExpressionType::Volume => {
                    tutti_clap_host::NoteExpressionType::Volume
                }
                tutti_plugin::server::NoteExpressionType::Pan => tutti_clap_host::NoteExpressionType::Pan,
                tutti_plugin::server::NoteExpressionType::Tuning => {
                    tutti_clap_host::NoteExpressionType::Tuning
                }
                tutti_plugin::server::NoteExpressionType::Vibrato => {
                    tutti_clap_host::NoteExpressionType::Vibrato
                }
                tutti_plugin::server::NoteExpressionType::Brightness => {
                    tutti_clap_host::NoteExpressionType::Brightness
                }
            };
            tutti_clap_host::NoteExpressionValue::new(expression_type, expr.note_id, expr.value)
                .at(expr.sample_offset)
        })
        .collect()
}

#[cfg(feature = "clap")]
fn convert_transport(
    transport: &tutti_plugin::server::TransportInfo,
    sample_rate: f64,
) -> tutti_clap_host::TransportInfo {
    let sr = if sample_rate.is_finite() && sample_rate > 0.0 {
        sample_rate
    } else {
        44100.0
    };
    tutti_clap_host::TransportInfo::default()
        .with_playing(transport.state.playing)
        .with_recording(transport.state.recording)
        .with_tempo(transport.timing.tempo)
        .with_time_signature(
            transport.timing.time_sig_numerator,
            transport.timing.time_sig_denominator,
        )
        .with_position_beats(
            transport.position.quarters,
            transport.position.samples as f64 / sr,
        )
        .with_loop(
            transport.state.cycle_active,
            transport.loop_region.start_quarters,
            transport.loop_region.end_quarters,
        )
        .with_bar(transport.bar.position_quarters, 0)
}

#[cfg(feature = "clap")]
fn convert_process_output(output: tutti_clap_host::instance::ProcessOutputRef<'_>) -> ProcessOutput {
    let midi_events: tutti_plugin::server::MidiEventVec =
        output.midi_events.iter().copied().collect();

    let mut param_changes = ParameterChanges::new();
    for queue in &output.param_changes.queues {
        let mut tutti_queue = tutti_plugin::server::ParameterQueue::new(queue.param_id);
        for point in &queue.points {
            tutti_queue.add_point(point.sample_offset, point.value);
        }
        param_changes.add_queue(tutti_queue);
    }

    let mut note_expression = NoteExpressionChanges::new();
    for expr in output.note_expressions {
        note_expression.add_change(tutti_plugin::server::NoteExpressionValue {
            sample_offset: expr.sample_offset,
            note_id: expr.note_id,
            expression_type: match expr.expression_type {
                tutti_clap_host::NoteExpressionType::Volume => {
                    tutti_plugin::server::NoteExpressionType::Volume
                }
                tutti_clap_host::NoteExpressionType::Pan => tutti_plugin::server::NoteExpressionType::Pan,
                tutti_clap_host::NoteExpressionType::Tuning => {
                    tutti_plugin::server::NoteExpressionType::Tuning
                }
                tutti_clap_host::NoteExpressionType::Vibrato => {
                    tutti_plugin::server::NoteExpressionType::Vibrato
                }
                tutti_clap_host::NoteExpressionType::Brightness => {
                    tutti_plugin::server::NoteExpressionType::Brightness
                }
                tutti_clap_host::NoteExpressionType::Pressure => {
                    tutti_plugin::server::NoteExpressionType::Volume
                } // Map to volume
                tutti_clap_host::NoteExpressionType::Expression => {
                    tutti_plugin::server::NoteExpressionType::Volume
                } // Map to volume
            },
            value: expr.value,
        });
    }

    ProcessOutput {
        midi_events,
        param_changes,
        note_expression,
    }
}

#[cfg(feature = "clap")]
fn convert_param_info(info: tutti_clap_host::ParameterInfo) -> ParameterInfo {
    let flags = ParameterFlags {
        automatable: info.flags.contains(tutti_clap_host::ParameterFlags::AUTOMATABLE),
        read_only: info.flags.contains(tutti_clap_host::ParameterFlags::READONLY),
        wrap: info.flags.contains(tutti_clap_host::ParameterFlags::PERIODIC),
        is_bypass: info.flags.contains(tutti_clap_host::ParameterFlags::BYPASS),
        hidden: info.flags.contains(tutti_clap_host::ParameterFlags::HIDDEN),
    };
    make_param_info(
        info.id,
        info.name,
        String::new(),
        info.min_value,
        info.max_value,
        info.default_value,
        0,
        flags,
    )
}

#[cfg(test)]
#[cfg(feature = "clap")]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use tutti_plugin::server::PluginInstance;
    use tutti_plugin::server::{
        NoteExpressionChanges, NoteExpressionType, NoteExpressionValue, ParameterChanges,
        ParameterQueue, TransportInfo,
    };

    const CLAP_PLUGIN: &str = "/Library/Audio/Plug-Ins/CLAP/TAL-NoiseMaker.clap";

    #[test]
    fn test_clap_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(CLAP_PLUGIN);
        let instance = ClapInstance::load(path, 44100.0, 512);
        assert!(
            instance.is_ok(),
            "Failed to load CLAP plugin: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        let meta = instance.metadata();
        assert!(!meta.name.is_empty(), "Plugin name should not be empty");
        assert!(!meta.id.is_empty(), "Plugin id should not be empty");
    }

    #[test]
    fn test_clap_metadata() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(CLAP_PLUGIN);
        let instance = ClapInstance::load(path, 44100.0, 512).expect("Failed to load CLAP plugin");
        let meta = instance.metadata();

        assert!(
            meta.audio_io.inputs > 0,
            "Expected audio inputs > 0, got {}",
            meta.audio_io.inputs
        );
        assert!(
            meta.audio_io.outputs > 0,
            "Expected audio outputs > 0, got {}",
            meta.audio_io.outputs
        );
        // NOTE: `supports_f64` is intentionally NOT asserted. f64 processing is
        // optional in CLAP (`CLAP_AUDIO_PORT_SUPPORTS_64BITS`) and rare in
        // practice — TAL-NoiseMaker reports f32-only. The flag must simply
        // reflect what the plugin advertises; both values are valid. Reading it
        // here confirms the metadata is populated without crashing.
        let _ = instance.metadata().supports_f64;
    }

    #[test]
    fn test_clap_parameter_count() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(CLAP_PLUGIN);
        let instance =
            ClapInstance::load(path, 44100.0, 512).expect("Failed to load CLAP plugin");

        let count = instance.get_parameter_list().len();
        assert!(
            count > 0,
            "TAL-NoiseMaker should have parameters, got {}",
            count
        );
    }

    #[test]
    fn test_clap_parameter_list() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(CLAP_PLUGIN);
        let instance =
            ClapInstance::load(path, 44100.0, 512).expect("Failed to load CLAP plugin");

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
    fn test_clap_get_parameter() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(CLAP_PLUGIN);
        let instance =
            ClapInstance::load(path, 44100.0, 512).expect("Failed to load CLAP plugin");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "Need at least one parameter");

        let first_id = params[0].id;
        let value = instance.get_parameter(first_id);
        assert!(
            value.is_finite(),
            "Parameter value should be finite, got {}",
            value
        );
    }

    #[test]
    fn test_clap_process_f32_silence() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(CLAP_PLUGIN);
        let mut instance =
            ClapInstance::load(path, 44100.0, 512).expect("Failed to load CLAP plugin");

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new();
        // Should not panic
        let _output = instance.process(tutti_plugin::server::AudioBufferMut::F32(buffer), &ctx);
    }

    // Note: f64 processing test removed — TAL-NoiseMaker's CLAP plugin crashes
    // when data32 is null (does not actually support f64-only processing).

    #[test]
    fn test_clap_process_f32_with_note() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(CLAP_PLUGIN);
        let mut instance =
            ClapInstance::load(path, 44100.0, 512).expect("Failed to load CLAP plugin");

        let num_samples = 512;

        // First block: send a NoteOn event
        let note_on = [tutti_plugin::server::MidiEvent::note_on(
            0,
            1,
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        )];

        let mut has_nonzero = false;

        // Process the block with NoteOn
        {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let ctx = ProcessContext::new().midi(&note_on);
            let _output = instance.process(tutti_plugin::server::AudioBufferMut::F32(buffer), &ctx);

            // Check output for non-zero samples
            for ch in output_data.iter() {
                for &sample in ch.iter() {
                    if sample != 0.0 {
                        has_nonzero = true;
                    }
                }
            }
        }

        // Process additional blocks (at least 4 more) to give the synth time to produce sound
        let empty_ctx = ProcessContext::new();
        for _ in 0..4 {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let _output = instance.process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &empty_ctx,
            );

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

    // ── Surge XT tests (f64-capable plugin) ──

    const SURGE_XT: &str = "/Library/Audio/Plug-Ins/CLAP/Surge XT.clap";

    #[test]
    fn test_surge_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(SURGE_XT), 44100.0, 512);
        assert!(
            instance.is_ok(),
            "Failed to load Surge XT: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        let meta = instance.metadata();
        assert!(!meta.name.is_empty());
        assert!(!meta.id.is_empty());
        println!("Surge XT loaded: {} ({})", meta.name, meta.id);
    }

    #[test]
    fn test_surge_port_flags() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(SURGE_XT), 44100.0, 512).expect("Failed to load Surge XT");

        let port_info = instance.loaded().audio_port_info(0, false);
        assert!(port_info.is_some(), "Expected at least one output port");

        let port = port_info.unwrap();
        assert!(port.channel_count >= 2, "Expected stereo output");
        // Note: most CLAP synths (including Surge XT) do not advertise
        // CLAP_AUDIO_PORT_SUPPORTS_64BITS. f64 processing is rare in practice.
    }

    #[test]
    fn test_surge_process_f32_silence() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(SURGE_XT), 44100.0, 512).expect("Failed to load Surge XT");

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new();
        let _output = instance.process(tutti_plugin::server::AudioBufferMut::F32(buffer), &ctx);
    }

    #[test]
    fn test_surge_process_f32_with_note() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(SURGE_XT), 44100.0, 512).expect("Failed to load Surge XT");

        let num_samples = 512;
        let note_on = [tutti_plugin::server::MidiEvent::note_on(
            0,
            1,
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        )];

        // Process block with NoteOn + follow-up blocks.
        // Surge XT is strict about CLAP thread contracts and may not produce
        // audio when process() is called from a non-audio thread, so we only
        // verify no crash rather than asserting on output content.
        for i in 0..5 {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let ctx = if i == 0 {
                ProcessContext::new().midi(&note_on)
            } else {
                ProcessContext::new()
            };

            let _output = instance.process(tutti_plugin::server::AudioBufferMut::F32(buffer), &ctx);
        }
    }

    /// Test Surge XT parameter setting via flush.
    #[test]
    fn test_surge_set_parameter_flush() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(SURGE_XT), 44100.0, 512).expect("Failed to load Surge XT");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "Need at least one parameter");

        let param_id = params[0].id;
        instance.set_parameter(param_id, 0.5);
    }

    // ── Group A: Plugin Lifecycle ──

    #[test]
    fn test_clap_lifecycle_active_after_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");
        // `load()` activates the plugin: `inner` is a `ClapActive` arm, so the
        // "active after load" guarantee is now encoded in the type itself.
        assert!(
            matches!(instance.inner, ClapInner::F32(_) | ClapInner::F64(_)),
            "load() should yield an active (ClapActive) instance"
        );
    }

    #[test]
    fn test_clap_lifecycle_processing_starts_on_process() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        // A freshly-activated instance has not started processing yet — that
        // happens lazily on the first `process` call (the type guarantees it is
        // already activated, so there is no "auto-activate" step to test).
        assert!(
            !instance.is_processing(),
            "Should not be processing before first process()"
        );

        let num_samples = 128;
        let input_data: Vec<Vec<f32>> = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();
        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };
        instance
            .process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &ProcessContext::new(),
            )
            .expect("process should succeed");

        assert!(
            instance.is_processing(),
            "Should be processing after first process()"
        );
    }

    #[test]
    fn test_clap_lifecycle_on_main_thread() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");
        // Just verify no crash
        instance.loaded_mut().on_main_thread();
    }

    // ── Group B: Polling Methods ──

    #[test]
    fn test_clap_poll_initially_clear() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        // All poll methods should return false on a fresh instance
        assert!(!instance.loaded().poll_restart_requested());
        assert!(!instance.loaded().poll_process_requested());
        assert!(!instance.loaded().poll_callback_requested());
        assert!(!instance.loaded().poll_latency_changed());
        assert!(!instance.loaded().poll_tail_changed());
        assert!(!instance.loaded().poll_params_rescan());
        assert!(!instance.loaded().poll_params_flush_requested());
        assert!(!instance.loaded().poll_state_dirty());
        assert!(!instance.loaded().poll_audio_ports_changed());
        assert!(!instance.loaded().poll_note_ports_changed());
    }

    #[test]
    fn test_clap_poll_restart() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let state = instance.loaded().host_state();
        state
            .lifecycle
            .restart_requested
            .store(true, Ordering::Release);

        // needs_restart() is non-clearing — should return true repeatedly
        assert!(
            instance.loaded().needs_restart(),
            "needs_restart should be true"
        );
        assert!(
            instance.loaded().needs_restart(),
            "needs_restart should still be true (non-clearing)"
        );

        // poll_restart_requested() clears the flag
        assert!(
            instance.loaded().poll_restart_requested(),
            "poll should return true"
        );
        assert!(
            !instance.loaded().poll_restart_requested(),
            "poll should return false after clearing"
        );
        assert!(
            !instance.loaded().needs_restart(),
            "needs_restart should be false after poll cleared it"
        );
    }

    #[test]
    fn test_clap_poll_process_callback() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let state = instance.loaded().host_state();
        state
            .lifecycle
            .process_requested
            .store(true, Ordering::Release);
        state
            .lifecycle
            .callback_requested
            .store(true, Ordering::Release);

        assert!(instance.loaded().poll_process_requested());
        assert!(instance.loaded().poll_callback_requested());

        // Second poll should be false (cleared)
        assert!(!instance.loaded().poll_process_requested());
        assert!(!instance.loaded().poll_callback_requested());
    }

    #[test]
    fn test_clap_poll_latency_tail() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let state = instance.loaded().host_state();
        state
            .processing
            .latency_changed
            .store(true, Ordering::Release);
        state.processing.tail_changed.store(true, Ordering::Release);

        assert!(instance.loaded().poll_latency_changed());
        assert!(instance.loaded().poll_tail_changed());

        assert!(!instance.loaded().poll_latency_changed());
        assert!(!instance.loaded().poll_tail_changed());
    }

    #[test]
    fn test_clap_poll_ports_state() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let state = instance.loaded().host_state();
        state.audio_ports.changed.store(true, Ordering::Release);
        state.notes.ports_changed.store(true, Ordering::Release);
        state.processing.state_dirty.store(true, Ordering::Release);

        assert!(instance.loaded().poll_audio_ports_changed());
        assert!(instance.loaded().poll_note_ports_changed());
        assert!(instance.loaded().poll_state_dirty());

        assert!(!instance.loaded().poll_audio_ports_changed());
        assert!(!instance.loaded().poll_note_ports_changed());
        assert!(!instance.loaded().poll_state_dirty());
    }

    // ── Group C: Parameter Get/Set Roundtrip ──

    #[test]
    fn test_clap_param_set_get_roundtrip() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "Need at least one parameter");

        let param_id = params[0].id;
        let original = instance.get_parameter(param_id);

        // Set to a different value
        let new_value = if original < 0.5 { 0.75 } else { 0.25 };
        instance.set_parameter(param_id, new_value);

        let readback = instance.get_parameter(param_id);
        assert!(
            (readback - new_value).abs() < 0.01,
            "Parameter should be close to set value: expected {}, got {}",
            new_value,
            readback
        );
    }

    // ── Group D: State Save/Load Roundtrip ──

    #[test]
    fn test_clap_state_save_nonempty() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let state = instance.get_state().expect("save_state should succeed");
        assert!(!state.is_empty(), "Saved state should not be empty");
    }

    #[test]
    fn test_clap_state_save_load_roundtrip() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "Need at least one parameter");
        let param_id = params[0].id;

        // Save original state
        let saved = instance.get_state().expect("save should succeed");
        let original_value = instance.get_parameter(param_id);

        // Change parameter
        let new_value = if original_value < 0.5 { 0.75 } else { 0.25 };
        instance.set_parameter(param_id, new_value);

        // Restore state
        instance.set_state(&saved).expect("restore should succeed");

        let restored_value = instance.get_parameter(param_id);
        assert!(
            (restored_value - original_value).abs() < 0.01,
            "Parameter should be restored: expected {}, got {}",
            original_value,
            restored_value
        );
    }

    // ── Group E: Audio/Note Port Enumeration ──

    #[test]
    fn test_clap_audio_port_enumeration() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let output_count = instance.loaded().audio_port_count(false);
        assert!(
            output_count > 0,
            "Synth should have at least one output port"
        );

        for i in 0..output_count {
            let info = instance
                .loaded()
                .audio_port_info(i, false)
                .expect("audio_port_info should return Some");
            assert!(info.channel_count > 0, "Port {} should have channels", i);
            assert!(!info.name.is_empty(), "Port {} should have a name", i);
        }
    }

    #[test]
    fn test_clap_note_port_enumeration() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let input_count = instance.loaded().note_port_count(true);
        assert!(
            input_count > 0,
            "Synth should have at least one note input port"
        );

        for i in 0..input_count {
            let info = instance
                .loaded()
                .note_port_info(i, true)
                .expect("note_port_info should return Some");
            assert!(!info.name.is_empty(), "Note port {} should have a name", i);
        }
    }

    // ── Group F: Latency/Tail Queries ──

    #[test]
    fn test_clap_latency_and_tail() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        // Just verify no crash — values depend on plugin
        let _latency = instance.loaded().get_latency();
        let _tail = instance.loaded().get_tail();
    }

    // ── Group G: Processing with Param Automation, Expressions, Transport ──

    #[test]
    fn test_clap_process_with_param_automation() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty());

        let mut changes = ParameterChanges::new();
        let mut queue = ParameterQueue::new(params[0].id);
        queue.add_point(0, 0.5);
        changes.add_queue(queue);

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new().params(&changes);
        let _output = instance.process(tutti_plugin::server::AudioBufferMut::F32(buffer), &ctx);
    }

    #[test]
    fn test_clap_process_with_note_expression() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let note_on = [tutti_plugin::server::MidiEvent::note_on(
            0,
            1,
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        )];

        let mut expr_changes = NoteExpressionChanges::new();
        expr_changes.add_change(NoteExpressionValue {
            sample_offset: 0,
            note_id: -1,
            expression_type: NoteExpressionType::Volume,
            value: 0.8,
        });

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new()
            .midi(&note_on)
            .note_expression(&expr_changes);
        let _output = instance.process(tutti_plugin::server::AudioBufferMut::F32(buffer), &ctx);
    }

    #[test]
    fn test_clap_process_with_transport() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let transport = TransportInfo::new()
            .with_playing(true)
            .with_tempo(120.0)
            .with_time_signature(4, 4);

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new().transport(&transport);
        let _output = instance.process(tutti_plugin::server::AudioBufferMut::F32(buffer), &ctx);
    }

    // ── Group I: Surge XT Additional Coverage ──

    #[test]
    fn test_surge_state_save_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(SURGE_XT), 44100.0, 512).expect("Failed to load Surge XT");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty());
        let param_id = params[0].id;

        let saved = instance.get_state().expect("save should succeed");
        assert!(!saved.is_empty(), "Surge XT state should not be empty");

        let original_value = instance.get_parameter(param_id);
        let new_value = if original_value < 0.5 { 0.75 } else { 0.25 };
        instance.set_parameter(param_id, new_value);

        instance.set_state(&saved).expect("restore should succeed");

        let restored = instance.get_parameter(param_id);
        assert!(
            (restored - original_value).abs() < 0.01,
            "Surge XT param should be restored: expected {}, got {}",
            original_value,
            restored
        );
    }

    #[test]
    fn test_surge_audio_ports() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(SURGE_XT), 44100.0, 512).expect("Failed to load Surge XT");

        let output_count = instance.loaded().audio_port_count(false);
        assert!(output_count > 0, "Surge XT should have output ports");

        let port = instance
                .loaded()
                .audio_port_info(0, false)
            .expect("Should have at least one output port");
        assert!(
            port.channel_count >= 2,
            "Expected stereo output, got {} channels",
            port.channel_count
        );
    }

    #[test]
    fn test_surge_latency_tail() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(SURGE_XT), 44100.0, 512).expect("Failed to load Surge XT");

        let _latency = instance.loaded().get_latency();
        let _tail = instance.loaded().get_tail();
    }

    // ── Group J: GUI / Editor ──

    #[test]
    fn test_clap_has_gui() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");
        assert!(
            instance.metadata().has_editor,
            "TAL-NoiseMaker should have a GUI"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore] // Requires main-thread Cocoa environment; run with: cargo test -- --ignored test_clap_gui
    fn test_clap_gui_open_close() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let parent = create_nsview();
        assert!(!parent.is_null(), "Failed to create NSView");

        let handle = unsafe { tutti_plugin::server::WindowHandle::from_u64(parent as u64) };
        let result = instance.open_editor(handle);
        assert!(
            result.is_ok(),
            "open_editor should succeed: {:?}",
            result.err()
        );

        let size = result.unwrap();
        assert!(
            size.width > 0 && size.height > 0,
            "GUI size should be non-zero: {}x{}",
            size.width,
            size.height
        );

        instance.close_editor();
        release_nsview(parent);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore] // Requires main-thread Cocoa environment; run with: cargo test -- --ignored test_surge_gui
    fn test_surge_gui_open_close() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(SURGE_XT), 44100.0, 512).expect("Failed to load Surge XT");

        assert!(instance.metadata().has_editor, "Surge XT should have a GUI");

        let parent = create_nsview();
        assert!(!parent.is_null());

        let handle = unsafe { tutti_plugin::server::WindowHandle::from_u64(parent as u64) };
        let result = instance.open_editor(handle);
        assert!(
            result.is_ok(),
            "Surge XT open_editor should succeed: {:?}",
            result.err()
        );

        let size = result.unwrap();
        assert!(
            size.width > 0 && size.height > 0,
            "Surge XT GUI size should be non-zero: {}x{}",
            size.width,
            size.height
        );

        instance.close_editor();
        release_nsview(parent);
    }

    // ── Group K: Render Mode ──

    #[test]
    fn test_clap_render_mode() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        // Set offline mode — returns true if plugin supports render extension
        let _ = instance.loaded_mut().set_render_mode(true);
        // Set back to realtime
        let _ = instance.loaded_mut().set_render_mode(false);
        // No crash is the assertion
    }

    #[test]
    fn test_clap_has_hard_realtime_requirement() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        // Most synths don't have hard RT requirements
        let _has_rt = instance.loaded().has_hard_realtime_requirement();
    }

    // ── Group L: Voice Info ──

    #[test]
    fn test_clap_voice_info() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        // TAL-NoiseMaker may or may not support voice info
        if let Some(info) = instance.loaded().get_voice_info() {
            assert!(info.voice_count > 0, "voice_count should be > 0");
            assert!(info.voice_capacity > 0, "voice_capacity should be > 0");
        }
    }

    #[test]
    fn test_surge_voice_info() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(SURGE_XT), 44100.0, 512).expect("Failed to load Surge XT");

        // Surge XT likely supports voice info
        if let Some(info) = instance.loaded().get_voice_info() {
            assert!(info.voice_count > 0, "Surge XT voice_count should be > 0");
            assert!(
                info.voice_capacity > 0,
                "Surge XT voice_capacity should be > 0"
            );
        }
    }

    // ── Group M: Note Names ──

    #[test]
    fn test_clap_note_names() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        let count = instance.loaded().note_name_count();
        // Iterate whatever's there — may be 0 for synths without custom note names
        for i in 0..count {
            let name = instance.loaded().get_note_name(i);
            assert!(name.is_some(), "note_name at index {} should exist", i);
            assert!(
                !name.unwrap().name.is_empty(),
                "note_name {} should have a name",
                i
            );
        }
    }

    // ── Group N: Sample Rate Cycling ──

    #[test]
    fn test_clap_sample_rate_change() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        // Change sample rate — this deactivates, updates, and requires reactivation
        instance.set_sample_rate(48000.0);

        // Process should still work (auto-reactivates)
        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 48000.0,
        };

        let ctx = ProcessContext::new();
        let _output = instance.process(tutti_plugin::server::AudioBufferMut::F32(buffer), &ctx);
    }

    #[test]
    fn test_clap_sample_rate_cycle_multiple() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        // Cycle through several sample rates
        for &rate in &[48000.0, 96000.0, 44100.0, 22050.0] {
            instance.set_sample_rate(rate);

            let num_samples = 512;
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];
            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: rate,
            };

            let ctx = ProcessContext::new();
            let _output = instance.process(tutti_plugin::server::AudioBufferMut::F32(buffer), &ctx);
        }
    }

    // ── Group O: State Context ──

    #[test]
    fn test_clap_state_context_support() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        // Just query — may or may not be supported
        let _supports = instance.loaded().supports_state_context();
    }

    #[test]
    fn test_clap_state_context_save_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        // Save with ForProject context (falls back to regular save if unsupported)
        let saved = instance
                .loaded()
                .state_with_context(tutti_clap_host::StateContext::ForProject)
            .expect("state_with_context should succeed");
        assert!(!saved.is_empty());

        // Load it back
        instance
            .loaded_mut()
            .set_state_with_context(&saved, tutti_clap_host::StateContext::ForProject)
            .expect("set_state_with_context should succeed");
    }

    #[test]
    fn test_clap_state_context_for_duplicate() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(CLAP_PLUGIN), 44100.0, 512).expect("Failed to load");

        // ForDuplicate context — used when duplicating a plugin instance
        let saved = instance
                .loaded()
                .state_with_context(tutti_clap_host::StateContext::ForDuplicate)
            .expect("save should succeed");
        assert!(!saved.is_empty());

        instance
            .loaded_mut()
            .set_state_with_context(&saved, tutti_clap_host::StateContext::ForDuplicate)
            .expect("load should succeed");
    }

    // Objective-C runtime FFI for NSView creation (macOS only).
    // On Apple Silicon, objc_msgSend must be cast to the correct function
    // pointer type — the variadic extern "C" declaration doesn't work.
    #[cfg(target_os = "macos")]
    extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn objc_msgSend();
    }

    #[cfg(target_os = "macos")]
    type ObjcMsgSendFn =
        unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void;

    #[cfg(target_os = "macos")]
    fn objc_send() -> ObjcMsgSendFn {
        unsafe { std::mem::transmute(objc_msgSend as *const ()) }
    }

    /// Ensure NSApplication is initialized (required for plugin GUIs).
    #[cfg(target_os = "macos")]
    fn ensure_nsapp() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| unsafe {
            let send = objc_send();
            let cls = objc_getClass(c"NSApplication".as_ptr());
            let shared_sel = sel_registerName(c"sharedApplication".as_ptr());
            send(cls, shared_sel);
        });
    }

    #[cfg(target_os = "macos")]
    fn create_nsview() -> *mut std::ffi::c_void {
        ensure_nsapp();
        unsafe {
            let send = objc_send();
            let cls = objc_getClass(c"NSView".as_ptr());
            let alloc_sel = sel_registerName(c"alloc".as_ptr());
            let init_sel = sel_registerName(c"init".as_ptr());

            let obj = send(cls, alloc_sel);
            send(obj, init_sel)
        }
    }

    #[cfg(target_os = "macos")]
    fn release_nsview(view: *mut std::ffi::c_void) {
        unsafe {
            let send = objc_send();
            let release_sel = sel_registerName(c"release".as_ptr());
            send(view, release_sel);
        }
    }
}
