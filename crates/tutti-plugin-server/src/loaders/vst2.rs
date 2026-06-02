//! VST2 plugin loader — thin wrapper around the `vst2-host` crate.
//!
//! Mirrors the same shim pattern as `loaders/au.rs`, `loaders/clap.rs`,
//! `loaders/vst3.rs`. All format-level work happens in `vst2-host`; this
//! file only adapts that crate's API to
//! [`tutti_plugin::server::PluginInstance`] and translates errors.

use std::path::Path;

use tutti_plugin::server::{
    AudioBufferMut, EditorSize, MidiEventVec, NoteExpressionChanges, ParameterChanges,
    ParameterInfo, PluginInfo, PluginInstance, ProcessContext, ProcessOutput, WindowHandle,
};
use tutti_plugin::{BridgeError, Result};

#[cfg(feature = "vst2")]
use tutti_vst2_host::{
    ProcessContext as Vst2ProcessContext, RenderScratch, Vst2Error, Vst2Instance as Vst2Host,
};

use crate::loaders::common::params::{make_param_info, ParamCache, ALL_AUTOMATABLE};

pub struct Vst2Instance {
    #[cfg(feature = "vst2")]
    inner: Vst2Host,
    #[cfg(feature = "vst2")]
    scratch: RenderScratch,
    metadata: PluginInfo,
    param_cache: ParamCache,
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
            let metadata = PluginInfo::new(host_meta.id, host_meta.name)
                .author(host_meta.vendor)
                .version(host_meta.version)
                .audio_io(host_meta.num_inputs, host_meta.num_outputs)
                .midi(host_meta.receives_midi)
                .f64_support(host_meta.supports_f64)
                .editor(host_meta.has_editor, None)
                .latency(host_meta.latency_samples);

            let scratch =
                RenderScratch::new(host_meta.num_inputs, host_meta.num_outputs, block_size);

            Ok(Self {
                inner,
                scratch,
                metadata,
                param_cache: ParamCache::default(),
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

    pub fn metadata(&self) -> &PluginInfo {
        &self.metadata
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
        Vst2Error::StateSaveError(s) => BridgeError::StateSaveError(s),
        Vst2Error::StateRestoreError(s) => BridgeError::StateRestoreError(s),
        Vst2Error::ProcessError(s) => BridgeError::ProcessError(s),
    }
}

impl PluginInstance for Vst2Instance {
    fn metadata(&self) -> &PluginInfo {
        &self.metadata
    }

    fn process(
        &mut self,
        buffer: AudioBufferMut<'_>,
        ctx: &ProcessContext,
    ) -> Result<ProcessOutput> {
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
            Err(BridgeError::ProcessError(
                "VST2 support not compiled".into(),
            ))
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

    fn get_parameter_list(&mut self) -> Vec<ParameterInfo> {
        #[cfg(feature = "vst2")]
        {
            self.inner
                .parameters()
                .into_iter()
                .map(|p| {
                    make_param_info(
                        p.id,
                        p.name,
                        p.unit,
                        0.0,
                        1.0,
                        p.current as f64,
                        0,
                        ALL_AUTOMATABLE,
                    )
                })
                .collect()
        }
        #[cfg(not(feature = "vst2"))]
        Vec::new()
    }

    fn get_parameter_info(&mut self, id: u32) -> Option<ParameterInfo> {
        let params = self.get_parameter_list();
        self.param_cache.lookup(id, || params)
    }

    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
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
                .map_err(|e| translate_error(e, Path::new("")))
        }
        #[cfg(not(feature = "vst2"))]
        {
            let _ = parent;
            Err(BridgeError::EditorError("VST2 support not compiled".into()))
        }
    }

    fn close_editor(&mut self) {
        #[cfg(feature = "vst2")]
        self.inner.close_editor();
    }

    fn editor_idle(&mut self) {
        #[cfg(feature = "vst2")]
        self.inner.editor_idle();
    }

    fn get_state(&mut self) -> Result<Vec<u8>> {
        #[cfg(feature = "vst2")]
        {
            self.inner
                .save_state()
                .map_err(|e| translate_error(e, Path::new("")))
        }
        #[cfg(not(feature = "vst2"))]
        Ok(Vec::new())
    }

    fn set_state(&mut self, data: &[u8]) -> Result<()> {
        #[cfg(feature = "vst2")]
        {
            self.inner
                .load_state(data)
                .map_err(|e| translate_error(e, Path::new("")))
        }
        #[cfg(not(feature = "vst2"))]
        {
            let _ = data;
            Ok(())
        }
    }
}
