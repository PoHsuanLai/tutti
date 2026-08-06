//! VST2 plugin loader — thin wrapper around the `vst2-host` crate.
//!
//! Mirrors the same shim pattern as `loaders/au.rs`, `loaders/clap.rs`,
//! `loaders/vst3.rs`. All format-level work happens in `vst2-host`; this
//! file only adapts that crate's API to
//! [`tutti_plugin::server::PluginInstance`] and translates errors.

use std::path::Path;

use tutti_plugin::server::{
    AudioBufferMut, EditorPresence, EditorSize, Features, LoadedPlugin, MidiEventVec, Normalized,
    NoteExpressionChanges, ParamAddress, ParameterChanges, ParameterInfo, PluginAudio, PluginClass,
    PluginDescriptor, PluginEditorHost, PluginMeta, PluginParams, PluginPresets, PluginResult,
    PluginState, PluginTail, Preset, PresetId, ProcessContext, ProcessOutput, RenderMode,
    WindowHandle,
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
                editor: EditorPresence::measured(host_meta.has_editor),
            };
            let mut features = Features::empty();
            // Backed by the render path: with this bit set, an `F64` buffer
            // reaches `processReplacingF64` rather than being narrowed.
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
            let probed = tutti_plugin::server::probed::VST2;

            // VST2 is single-bus: one main input bus, one main output bus.
            let loaded = LoadedPlugin {
                // Already `ChannelLayout`s on the host meta — passed through
                // rather than degraded to a count for `single_bus` to rebuild.
                inputs: single_bus(host_meta.num_inputs),
                outputs: single_bus(host_meta.num_outputs),
                latency_samples: host_meta.latency_samples,
                // `Unknown`, not `None`: nothing here has asked. Claiming
                // `None` would tell a bounce to add nothing, which is wrong for
                // every VST2 reverb.
                //
                // The dispatch does exist — `Host::get_tail_size` in
                // `vst-tutti` sends `OpCode::GetTailSize` — but `Vst2Instance`
                // does not re-export it, so it is not reachable from here yet.
                //
                // Wiring it is not a one-liner, because VST2 encodes tail
                // inversely to every other format: `0` means "no tail
                // information, host decides" and `1` means "no tail at all", so
                // the two ends `from_samples` maps are swapped and its `0 =>
                // None` arm would read "unknown" as "silent".
                tail: PluginTail::Unknown,
                features,
                probed,
                // VST2 reports no speaker placement. `effSetSpeakerArrangement`
                // (opcode 42) exists, but the `VstSpeakerArrangement` struct its
                // ABI needs is not defined anywhere in the vendored bindings and
                // the host never sends it — see D-11 in the plugin-host audit.
                // Empty lists claim nothing about any bus, which is the honest
                // answer for a format that cannot be asked.
                input_topology: Default::default(),
                output_topology: Default::default(),
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

impl PluginMeta for Vst2Instance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.meta.loaded
    }
}

impl PluginAudio for Vst2Instance {
    fn process(
        &mut self,
        buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
    ) -> PluginResult<ProcessOutput> {
        #[cfg(feature = "vst2")]
        {
            // VST3/CLAP-style param change events become direct writes for VST2.
            // VST2 `set_parameter` expects a normalized `0..1` value, which is
            // exactly the host's authoring convention — clamp defensively so an
            // over-range authored/modulated value can't leave `[0, 1]`.
            if let Some(changes) = ctx.param_changes {
                for queue in &changes.queues {
                    if let Some(point) = queue.points.last() {
                        // VST2 is the one format addressed by position, so an
                        // opaque handle addresses nothing here. This used to
                        // narrow the wire's bare `u32` with `i32::try_from`,
                        // rebuilding an index the producer already knew it was
                        // sending — the queue now says so.
                        let Some(index) = queue.param_id.index() else {
                            continue;
                        };
                        // Already on the unit interval: `Normalized` cannot
                        // be built otherwise. The `.clamp(0.0, 1.0)` that stood
                        // here was one of four copies of that guard and was the
                        // unsafe spelling — `f32::clamp` returns NaN for a NaN
                        // input, so a NaN automation point reached the plugin
                        // through the very call that looked like it stopped it.
                        self.inner.set_parameter(index, point.value.get() as f32);
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

    /// Store the level the host reports through
    /// `audioMasterGetCurrentProcessLevel`.
    ///
    /// Always accepted: VST2 has no query a plugin could decline — the host
    /// answers whenever the plugin asks. So unlike CLAP and AU this reports
    /// `true` unconditionally, and `probed::VST2` carries `RENDER_MODE` for the
    /// same reason.
    fn set_render_mode(&mut self, mode: RenderMode) -> bool {
        #[cfg(feature = "vst2")]
        self.inner.set_offline_render(mode.is_offline());
        #[cfg(not(feature = "vst2"))]
        let _ = mode;
        true
    }
}

impl PluginParams for Vst2Instance {
    fn get_parameter(&self, id: ParamAddress) -> f64 {
        #[cfg(feature = "vst2")]
        {
            // `PluginParams` is the shared cross-format vocabulary and returns a
            // bare `f64`, so the VST2-specific "plugin exposes no accessor" case
            // is flattened here at the boundary rather than propagated. 0.0
            // matches what the other format loaders return for an unreadable
            // parameter; the distinction stays available on `Vst2Instance`.
            // VST2 is the one format addressed by position, so an opaque
            // handle addresses nothing here. `Vst2Instance::parameter` bounds-
            // checks the index it is given; see `param_index` there.
            id.index()
                .and_then(|i| self.inner.parameter(i))
                .unwrap_or(0.0) as f64
        }
        #[cfg(not(feature = "vst2"))]
        {
            let _ = id;
            0.0
        }
    }

    fn set_parameter(&mut self, id: ParamAddress, value: Normalized) {
        // Same boundary flattening: the shared trait returns `()`, so a write
        // the plugin cannot accept — including one addressed by an opaque
        // handle — is dropped here rather than reported.
        #[cfg(feature = "vst2")]
        let _ = id
            .index()
            .map(|i| self.inner.set_parameter(i, value.get() as f32));
        #[cfg(not(feature = "vst2"))]
        let _ = (id, value);
    }

    fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        #[cfg(feature = "vst2")]
        {
            // The narrow→shared mapping lives on the host crate's
            // `Vst2Instance::parameter_list`; the in-process VST2 backend's
            // `HostParams` impl calls the same helper, so there is one VST2 param map.
            self.inner.parameter_list()
        }
        #[cfg(not(feature = "vst2"))]
        Vec::new()
    }
}

impl PluginEditorHost for Vst2Instance {
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
}

impl PluginPresets for Vst2Instance {
    /// VST2 programs. The index **is** the identifier — `effProgramChange`
    /// takes a position in `[0, numPrograms)` — so unlike AU's sparse selectors
    /// these are genuinely dense. Unnamed slots are kept: dropping one would
    /// renumber every program after it.
    #[cfg(feature = "vst2")]
    fn get_presets(&mut self) -> Vec<Preset> {
        self.inner
            .programs()
            .into_iter()
            .map(|(index, name)| Preset::new(PresetId::Number(index), name))
            .collect()
    }

    /// Switch program, bracketed by `effBeginSetProgram`/`effEndSetProgram` in
    /// the host layer. `false` for an id this format cannot address.
    #[cfg(feature = "vst2")]
    fn load_preset(&mut self, id: &PresetId) -> bool {
        match id.number() {
            Some(index) => self.inner.set_program(index),
            None => false,
        }
    }

    #[cfg(feature = "vst2")]
    fn get_current_preset(&mut self) -> Option<PresetId> {
        Some(PresetId::Number(self.inner.current_program()))
    }
}

impl PluginState for Vst2Instance {
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
