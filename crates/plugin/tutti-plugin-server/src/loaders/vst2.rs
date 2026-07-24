//! VST2 plugin loader — thin wrapper around the `vst2-host` crate.
//!
//! Mirrors the same shim pattern as `loaders/au.rs`, `loaders/clap.rs`,
//! `loaders/vst3.rs`. All format-level work happens in `vst2-host`; this
//! file only adapts that crate's API to
//! [`tutti_plugin::server::PluginInstance`] and translates errors.

use std::path::Path;

use tutti_plugin::server::{
    AudioBufferMut, EditorSize, Features, LoadedPlugin, MidiEventVec, NoteExpressionChanges,
    ParameterChanges, ParameterInfo, PluginClass, PluginDescriptor, PluginInstance, PluginResult,
    ProcessContext, ProcessOutput, WindowHandle,
};
// Only the `not(vst2)` fallback arms construct `PluginError` directly.
#[cfg(not(feature = "vst2"))]
use tutti_plugin::server::PluginError;
use tutti_plugin::{BridgeError, Result};

#[cfg(feature = "vst2")]
use tutti_vst2_host::{
    ProcessContext as Vst2ProcessContext, RenderScratch, Vst2Error, Vst2Instance as Vst2Host,
};

use crate::loaders::common::{single_bus, Meta};

pub struct Vst2Instance {
    #[cfg(feature = "vst2")]
    inner: Vst2Host,
    #[cfg(feature = "vst2")]
    scratch: RenderScratch,
    meta: Meta,
    #[allow(dead_code)] // Carried for diagnostics under the not(vst2) cfg.
    sample_rate: f64,
}

impl Vst2Instance {
    pub fn load(path: &Path, sample_rate: f64, block_size: usize) -> Result<Self> {
        #[cfg(feature = "vst2")]
        {
            let inner = Vst2Host::load(path, sample_rate, block_size)
                .map_err(|e| translate_error(e, path))?;

            let host_meta = inner.metadata().clone();
            let descriptor = PluginDescriptor {
                id: host_meta.id,
                name: host_meta.name,
                vendor: host_meta.vendor,
                version: host_meta.version,
                class: PluginClass::Vst2 {
                    category: host_meta.category,
                },
                has_editor: host_meta.has_editor,
            };
            let mut features = Features::empty();
            // VST2's advertised f64 is informational only (the `vst` crate is
            // f32-internally), but the flag reflects what the plugin declares.
            features.set(Features::F64_AUDIO, host_meta.supports_f64);
            features.set(Features::MIDI_IN, host_meta.receives_midi);
            // MIDI-out is the plugin's declared output-bus count, not the
            // combined input flag — the host drains only what the plugin emits.
            features.set(Features::MIDI_OUT, host_meta.emits_midi);
            features.set(Features::EDITOR, host_meta.has_editor);
            // VST2 always gets a transport snapshot (get_time_info). No
            // sample-accurate automation, note-expression, sequencer context, or
            // host-driven editor resize (fused AEffect editor, no sizeWindow).
            features.insert(Features::TRANSPORT);

            // VST2 is single-bus: one main input bus, one main output bus.
            let loaded = LoadedPlugin {
                inputs: single_bus(host_meta.num_inputs.count() as usize),
                outputs: single_bus(host_meta.num_outputs.count() as usize),
                latency_samples: host_meta.latency_samples,
                features,
            };

            let scratch =
                RenderScratch::new(host_meta.num_inputs, host_meta.num_outputs, block_size);

            Ok(Self {
                inner,
                scratch,
                meta: Meta { descriptor, loaded },
                sample_rate,
            })
        }

        #[cfg(not(feature = "vst2"))]
        {
            let _ = block_size;
            Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: tutti_plugin::LoadStage::Opening,
                reason: "VST2 support not compiled (enable 'vst2' feature)".into(),
            })
        }
    }

    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    /// Drain plugin-internal parameter changes (e.g., GUI knob movement).
    /// Called by `crate::plugin::Plugin::poll_async_events`.
    pub fn poll_parameter_changes(&self) -> Vec<(i32, f32)> {
        #[cfg(feature = "vst2")]
        {
            self.inner.drain_param_changes()
        }
        #[cfg(not(feature = "vst2"))]
        Vec::new()
    }
}

#[cfg(feature = "vst2")]
fn translate_error(err: Vst2Error, _path: &Path) -> BridgeError {
    match err {
        Vst2Error::LoadFailed {
            path,
            stage,
            reason,
        } => BridgeError::LoadFailed {
            path,
            // Host and bridge LoadStage are now the same shared type
            // (tutti-plugin-types); no conversion needed.
            stage,
            reason,
        },
        Vst2Error::EditorError(s) => BridgeError::EditorError(s),
        Vst2Error::StateRestoreError(s) => BridgeError::StateRestoreError(s),
    }
}

impl PluginInstance for Vst2Instance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.meta.loaded
    }

    fn process(
        &mut self,
        buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
    ) -> PluginResult<ProcessOutput> {
        #[cfg(feature = "vst2")]
        {
            // VST3/CLAP-style param change events become direct writes for VST2.
            if let Some(changes) = ctx.param_changes {
                for queue in &changes.queues {
                    if let Some(point) = queue.points.last() {
                        self.inner.set_parameter(queue.param_id, point.value as f32);
                    }
                }
            }

            let transport = ctx.transport.cloned();

            let midi_out: MidiEventVec = match buffer {
                AudioBufferMut::F32(buf) => {
                    let host_ctx = Vst2ProcessContext {
                        midi: ctx.midi_events,
                        transport: transport.as_ref(),
                        sample_rate: buf.sample_rate,
                    };
                    self.inner
                        .process_f32(
                            buf.inputs,
                            buf.outputs,
                            buf.num_samples,
                            &host_ctx,
                            &mut self.scratch,
                        )
                        .iter()
                        .copied()
                        .collect()
                }
                AudioBufferMut::F64(buf) => {
                    let host_ctx = Vst2ProcessContext {
                        midi: ctx.midi_events,
                        transport: transport.as_ref(),
                        sample_rate: buf.sample_rate,
                    };
                    self.inner
                        .process_f64(
                            buf.inputs,
                            buf.outputs,
                            buf.num_samples,
                            &host_ctx,
                            &mut self.scratch,
                        )
                        .iter()
                        .copied()
                        .collect()
                }
            };

            Ok(ProcessOutput {
                midi_events: midi_out,
                param_changes: ParameterChanges::new(),
                note_expression: NoteExpressionChanges::new(),
            })
        }

        #[cfg(not(feature = "vst2"))]
        {
            let _ = (buffer, ctx);
            Err(PluginError::Process("VST2 support not compiled".into()))
        }
    }

    fn set_sample_rate(&mut self, rate: f64) {
        self.sample_rate = rate;
        #[cfg(feature = "vst2")]
        self.inner.set_sample_rate(rate);
    }

    fn get_parameter(&self, id: u32) -> f64 {
        #[cfg(feature = "vst2")]
        {
            self.inner.parameter(id) as f64
        }
        #[cfg(not(feature = "vst2"))]
        {
            let _ = id;
            0.0
        }
    }

    fn set_parameter(&mut self, id: u32, value: f64) {
        #[cfg(feature = "vst2")]
        self.inner.set_parameter(id, value as f32);
        #[cfg(not(feature = "vst2"))]
        let _ = (id, value);
    }

    fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        #[cfg(feature = "vst2")]
        {
            // The narrow→shared mapping lives on the host crate's
            // `Vst2Instance::parameter_list`; the in-process ControlBackend
            // calls the same helper, so there is one VST2 param map.
            self.inner.parameter_list()
        }
        #[cfg(not(feature = "vst2"))]
        Vec::new()
    }

    fn open_editor(&mut self, parent: WindowHandle) -> PluginResult<EditorSize> {
        #[cfg(feature = "vst2")]
        {
            // Adapt tutti-plugin's WindowHandle (own pointer) to vst2-host's
            // local WindowHandle wrapper.
            let host_parent = unsafe { tutti_vst2_host::WindowHandle::from_ptr(parent.as_ptr()) };
            self.inner
                .open_editor(host_parent)
                .map(|sz| EditorSize {
                    width: sz.width,
                    height: sz.height,
                })
                .map_err(|e| translate_error(e, Path::new("")).into())
        }
        #[cfg(not(feature = "vst2"))]
        {
            let _ = parent;
            Err(PluginError::Editor(tutti_plugin::EditorError::PluginError(
                "VST2 support not compiled".into(),
            )))
        }
    }

    fn close_editor(&mut self) {
        #[cfg(feature = "vst2")]
        self.inner.close_editor();
    }

    fn get_state(&mut self) -> PluginResult<Vec<u8>> {
        #[cfg(feature = "vst2")]
        {
            self.inner
                .save_state()
                .map_err(|e| translate_error(e, Path::new("")).into())
        }
        #[cfg(not(feature = "vst2"))]
        Ok(Vec::new())
    }

    fn set_state(&mut self, data: &[u8]) -> PluginResult<()> {
        #[cfg(feature = "vst2")]
        {
            self.inner
                .load_state(data)
                .map_err(|e| translate_error(e, Path::new("")).into())
        }
        #[cfg(not(feature = "vst2"))]
        {
            let _ = data;
            Ok(())
        }
    }
}
