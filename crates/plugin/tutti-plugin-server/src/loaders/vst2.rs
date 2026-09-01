//! VST2 plugin loader — thin wrapper around the `vst2-host` crate.
//!
//! Mirrors the same shim pattern as `loaders/au.rs`, `loaders/clap.rs`,
//! `loaders/vst3.rs`. All format-level work happens in `vst2-host`; this
//! file only adapts that crate's API to
//! [`tutti_plugin::server::PluginInstance`] and translates errors.

use std::path::Path;

use tutti_plugin::server::{
    AudioBufferMut, EditorPresence, EditorSize, Features, LoadedPlugin, Normalized, ParamAddress,
    ParameterInfo, PluginAudio, PluginClass, PluginDescriptor, PluginEditorHost, PluginMeta,
    PluginParams, PluginPresets, PluginResult, PluginState, Preset, PresetId, ProcessContext,
    ProcessOutput, RenderMode, WindowHandle,
};
// Only the `not(vst2)` fallback arms construct `PluginError` directly.
#[cfg(not(feature = "vst2"))]
use tutti_plugin::server::PluginError;
use tutti_plugin::{BridgeError, Result};

#[cfg(feature = "vst2")]
use tutti_vst2_host::{
    RenderScratch, Vst2Error, Vst2Instance as Vst2Host, Vst2ProcessContext,
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
                // Asked and decoded, rather than the `Unknown` this used to
                // hardcode. The decode is `tutti-vst2-host`'s `decode_tail` and
                // deliberately not `PluginTail::from_samples`: VST2 inverts the
                // convention, so `0` is "no information" where every other
                // format means "no tail". `Unknown` remains the answer for a
                // plugin that declines the opcode.
                tail: host_meta.tail,
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
        out: &mut ProcessOutput,
    ) -> PluginResult<()> {
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
                        // opaque handle addresses nothing here. Take the index
                        // the queue carries rather than narrowing a bare `u32`
                        // back into one the producer already knew it was
                        // sending.
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

            // `process_f32` / `process_f64` return a borrow into the instance's
            // own pooled MIDI-out buffer. Extending `out` — which the caller
            // cleared and whose capacity survives the block — copies out of it
            // without a fresh `SmallVec`, which is what `.collect()` built here
            // every block and heap-spilled past its inline capacity.
            let emitted = match buffer {
                AudioBufferMut::F32(buf) => {
                    let host_ctx = Vst2ProcessContext {
                        midi: ctx.midi_events,
                        transport: transport.as_ref(),
                        sample_rate: buf.sample_rate,
                    };
                    self.inner.process_f32(
                        buf.inputs,
                        buf.outputs,
                        buf.num_samples,
                        &host_ctx,
                        &mut self.scratch,
                    )
                }
                AudioBufferMut::F64(buf) => {
                    let host_ctx = Vst2ProcessContext {
                        midi: ctx.midi_events,
                        transport: transport.as_ref(),
                        sample_rate: buf.sample_rate,
                    };
                    self.inner.process_f64(
                        buf.inputs,
                        buf.outputs,
                        buf.num_samples,
                        &host_ctx,
                        &mut self.scratch,
                    )
                }
            };
            out.midi_events.extend(emitted.iter().copied());

            // VST2 reports parameter automation through the `audioMasterAutomate`
            // callback, not through a per-block output list, so there is nothing
            // to fill `param_changes` from here. It reaches the host by the
            // separate `drain_param_changes` path.
            Ok(())
        }

        #[cfg(not(feature = "vst2"))]
        {
            let _ = (buffer, ctx, out);
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
            // handle addresses nothing here. `Vst2Instance::get_parameter` bounds-
            // checks the index it is given; see `param_index` there.
            id.index()
                .and_then(|i| self.inner.get_parameter(i))
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

    /// The plugin's display string, but **only for the value it currently
    /// holds**.
    ///
    /// `effGetParamDisplay` passes the plugin an index and nothing else, so it
    /// formats its own current value; VST 2.4 has no call that formats an
    /// arbitrary one. The other three formats take the value as an argument.
    ///
    /// So this answers when `value` is the value the plugin is already at, and
    /// `None` otherwise. Setting the parameter in order to read its label would
    /// make a display query *audible* and would race whatever else is writing
    /// that parameter — a text lookup must not move a knob. Returning the
    /// current value's string regardless would be worse still: it is the wrong
    /// answer, presented as the right one, and a caller comparing two values
    /// would see the same label for both.
    ///
    /// The comparison is exact rather than epsilon'd. Both sides come from the
    /// same `f32` the plugin holds — `parameter` reads it back and
    /// `Normalized::get` did not rescale it — so a value that round-tripped
    /// through this seam compares bit-equal, and one that did not is not a value
    /// the plugin can describe anyway.
    fn parameter_text(&self, id: ParamAddress, value: Normalized) -> Option<String> {
        #[cfg(feature = "vst2")]
        {
            let index = id.index()?;
            let current = self.inner.get_parameter(index)?;
            (f64::from(current) == value.get())
                .then(|| self.inner.parameter_display(index))
                .flatten()
        }
        #[cfg(not(feature = "vst2"))]
        {
            let _ = (id, value);
            None
        }
    }

    /// Parse `text` through the plugin's own `effString2Parameter`.
    ///
    /// **This writes.** The VST2 opcode is a setter with no parse-only
    /// counterpart, so the parameter lands on the parsed value as a side effect
    /// of asking. That is a real divergence from the other three formats, where
    /// this method is a pure query; it is surfaced here rather than hidden
    /// because the alternative — refusing to implement it — would leave a user
    /// unable to type a value into a VST2 field at all.
    ///
    /// The value is read back after the write, since the opcode reports only
    /// acceptance. Already normalized, as all VST2 parameter values are.
    fn parameter_value_from_text(&self, id: ParamAddress, text: &str) -> Option<Normalized> {
        #[cfg(feature = "vst2")]
        {
            let index = id.index()?;
            let value = self.inner.set_parameter_from_string(index, text)?;
            Some(Normalized::new(f64::from(value)))
        }
        #[cfg(not(feature = "vst2"))]
        {
            let _ = (id, text);
            None
        }
    }

    fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        #[cfg(feature = "vst2")]
        {
            // The narrow→shared mapping lives on the host crate's
            // `Vst2Instance::get_parameter_list`; the in-process VST2 backend's
            // `HostParams` impl calls the same helper, so there is one VST2 param map.
            self.inner.get_parameter_list()
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
                .get_state()
                .map_err(|e| translate_error(e, Path::new("")).into())
        }
        #[cfg(not(feature = "vst2"))]
        Ok(Vec::new())
    }

    fn set_state(&mut self, data: &[u8]) -> PluginResult<()> {
        #[cfg(feature = "vst2")]
        {
            self.inner
                .set_state(data)
                .map_err(|e| translate_error(e, Path::new("")).into())
        }
        #[cfg(not(feature = "vst2"))]
        {
            let _ = data;
            Ok(())
        }
    }
}

#[cfg(all(test, feature = "vst2"))]
mod tests {
    use super::*;
    use tutti_plugin::server::PluginTail;

    /// Every `TUTTI_VST2_PROBE_*` key the reference plugin reads at
    /// construction, so one test's configuration cannot leak into the next.
    /// Listed rather than derived: the probe owns the set, and a key added
    /// there without being added here would leak silently.
    const PROBE_ENV_KEYS: &[&str] = &[
        "TUTTI_VST2_PROBE_EDITOR",
        "TUTTI_VST2_PROBE_EFFECT_NAME",
        "TUTTI_VST2_PROBE_INPUTS",
        "TUTTI_VST2_PROBE_IS_SYNTH",
        "TUTTI_VST2_PROBE_LATENCY",
        "TUTTI_VST2_PROBE_MIDI_INPUTS",
        "TUTTI_VST2_PROBE_MIDI_OUTPUTS",
        "TUTTI_VST2_PROBE_NO_CAN_REPLACING",
        "TUTTI_VST2_PROBE_NO_CHUNKS",
        "TUTTI_VST2_PROBE_OUTPUTS",
        "TUTTI_VST2_PROBE_PARAMS",
        "TUTTI_VST2_PROBE_PROGRAMS",
        "TUTTI_VST2_PROBE_SERVICED_PARAMS",
        "TUTTI_VST2_PROBE_SERVICED_PROGRAMS",
        "TUTTI_VST2_PROBE_TAIL_SIZE",
    ];

    fn clear_probe_env() {
        for key in PROBE_ENV_KEYS {
            // SAFETY: every caller holds the plugin-load lock, so no other test
            // thread is reading or writing the environment concurrently.
            unsafe { std::env::remove_var(key) };
        }
    }

    /// Load the reference plugin with `env` applied at construction.
    ///
    /// The probe reads its metadata once, while the `AEffect` is being built,
    /// so the variables must be set before `load` and are cleared immediately
    /// after — leaving them set would configure whichever test ran next.
    ///
    /// Returns the guard alongside the instance: VST2 loading is serialized on
    /// a global in the `vst` crate, and dropping the guard early would let a
    /// second load race this one.
    fn load_probe(env: &[(&str, &str)]) -> (Vst2Instance, std::sync::MutexGuard<'static, ()>) {
        let lock = crate::test_utils::plugin_load_lock();
        clear_probe_env();
        for (k, v) in env {
            // SAFETY: as in `clear_probe_env` — the lock is held.
            unsafe { std::env::set_var(k, v) };
        }
        let path = crate::test_utils::vst2_probe_path();
        let loaded = Vst2Instance::load(Path::new(path), 48_000.0, 512);
        clear_probe_env();
        (
            loaded.unwrap_or_else(|e| panic!("loading the VST2 probe at {path} failed: {e:?}")),
            lock,
        )
    }

    /// The declared channel counts reach `LoadedPlugin` as one bus per side.
    ///
    /// The loader's own comment calls VST2 "single-bus", and this is what that
    /// claim means downstream: a host sizing buffers reads one width per side.
    /// Asserting the *width* as well as the bus count is what fails if
    /// `single_bus` is ever handed a count where a layout was meant — the two
    /// are both integers, so a swap compiles.
    #[test]
    fn declared_channel_counts_arrive_as_one_bus_per_side() {
        let (p, _lock) = load_probe(&[
            ("TUTTI_VST2_PROBE_INPUTS", "2"),
            ("TUTTI_VST2_PROBE_OUTPUTS", "2"),
        ]);
        let loaded = p.loaded();

        assert_eq!(loaded.inputs.len(), 1, "VST2 has exactly one input bus");
        assert_eq!(loaded.outputs.len(), 1, "VST2 has exactly one output bus");
        assert_eq!(loaded.inputs[0].count(), 2, "input width");
        assert_eq!(loaded.outputs[0].count(), 2, "output width");
    }

    /// A width other than stereo survives the mapping.
    ///
    /// The stereo case above would pass against a loader that hard-coded 2, so
    /// it cannot see a mapping that ignores what the plugin declared.
    #[test]
    fn a_non_stereo_width_is_carried_rather_than_assumed() {
        let (p, _lock) = load_probe(&[
            ("TUTTI_VST2_PROBE_INPUTS", "1"),
            ("TUTTI_VST2_PROBE_OUTPUTS", "4"),
        ]);
        let loaded = p.loaded();

        assert_eq!(loaded.inputs[0].count(), 1, "mono in");
        assert_eq!(loaded.outputs[0].count(), 4, "quad out");
    }

    /// Both MIDI directions reach `Features` from the host's own resolution.
    ///
    /// Deliberately *not* asserted from the pin counts: `tutti-vst2-host`
    /// resolves each direction as "the plugin's `can_do` answer, falling back
    /// to pin counts only on `Maybe`", and this probe answers `Yes`. So a
    /// `MIDI_OUTPUTS=0` build still reports `MIDI_OUT`, correctly — an explicit
    /// yes outranks an inference. (An earlier draft of this test asserted the
    /// opposite and failed; the resolution order is the reason, and it is
    /// deliberate.)
    ///
    /// What this pins is the seam the loader owns: two independent host fields
    /// reaching two independent bits. The `Maybe`-inference path underneath is
    /// driven through `dlopen`-set switches, so it belongs to
    /// `tutti-vst2-host`'s integration suite rather than here.
    #[test]
    fn both_midi_directions_reach_features() {
        let (p, _lock) = load_probe(&[
            ("TUTTI_VST2_PROBE_MIDI_INPUTS", "16"),
            ("TUTTI_VST2_PROBE_MIDI_OUTPUTS", "16"),
        ]);
        let f = p.loaded().features;

        assert!(f.contains(Features::MIDI_IN), "declared MIDI inputs");
        assert!(f.contains(Features::MIDI_OUT), "declared MIDI outputs");
    }

    /// Transport is set unconditionally, editor only when the plugin has one.
    ///
    /// Paired deliberately: `TRANSPORT` is `insert`ed with no condition (every
    /// VST2 gets `get_time_info`), while `EDITOR` is measured. Asserting both
    /// against one plugin that has no editor is what catches the two being
    /// wired to the same source.
    #[test]
    fn transport_is_unconditional_but_editor_is_measured() {
        let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_EDITOR", "0")]);
        let f = p.loaded().features;

        assert!(
            f.contains(Features::TRANSPORT),
            "every VST2 is handed a transport snapshot"
        );
        assert!(
            !f.contains(Features::EDITOR),
            "this probe declares no editor"
        );
    }

    /// An editor-bearing probe sets the bit, so the assertion above is real.
    #[test]
    fn an_editor_bearing_plugin_reports_one() {
        let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_EDITOR", "1")]);
        assert!(p.loaded().features.contains(Features::EDITOR));
    }

    /// `effGetTailSize` is asked, and its **inverted** encoding decoded.
    ///
    /// VST2 is the one format whose `0` does not mean "no tail":
    ///
    /// | raw | meaning                             | decoded     |
    /// |-----|-------------------------------------|-------------|
    /// | `0` | no tail *information*; host decides | `Unknown`   |
    /// | `1` | no tail at all                      | `None`      |
    /// | `n` | `n` samples of ring-out             | `Finite(n)` |
    ///
    /// Reading a raw `0` as `None` — which `PluginTail::from_samples` would,
    /// since that is right for CLAP and VST3 — tells a bounce to add no decay
    /// and truncates every reverb. All three rows are asserted together
    /// because the bug is a *swap*: either zero alone still passes half a
    /// table.
    ///
    /// `TUTTI_VST2_PROBE_TAIL_SIZE` sets the **raw wire value**; the probe
    /// intercepts the opcode before vst-rs can rewrite a trait-reported `0`
    /// into `1`, which is what makes the `0` row testable at all.
    #[test]
    fn the_inverted_tail_encoding_is_decoded_not_passed_through() {
        for (raw, want, why) in [
            (
                "0",
                PluginTail::Unknown,
                "raw 0 is 'no information', not a declared silence",
            ),
            ("1", PluginTail::None, "raw 1 is VST2's 'no tail at all'"),
            (
                "48000",
                PluginTail::Finite(tutti_plugin::server::Samples(48_000)),
                "anything above 1 is a sample count",
            ),
        ] {
            let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_TAIL_SIZE", raw)]);
            assert_eq!(p.loaded().tail, want, "raw={raw}: {why}");
        }
    }

    /// A negative answer is read as `Unknown`, not clamped to a silence.
    ///
    /// VST2 gives no meaning to a negative tail. Clamping to `0` would land on
    /// whichever verdict `0` maps to, making a malformed answer
    /// indistinguishable from a deliberate one — and the probe exists to
    /// misbehave in exactly this way.
    #[test]
    fn a_negative_tail_is_unknown_rather_than_clamped() {
        let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_TAIL_SIZE", "-1")]);
        assert_eq!(p.loaded().tail, PluginTail::Unknown);
    }

    /// Speaker topology is empty for both sides.
    ///
    /// VST2 cannot be asked — the `VstSpeakerArrangement` struct is not in the
    /// vendored bindings — so empty lists claim nothing rather than asserting
    /// a placement the format never reported.
    #[test]
    fn no_speaker_topology_is_claimed() {
        let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_OUTPUTS", "6")]);
        let loaded = p.loaded();

        assert!(
            loaded.input_topology.is_empty() && loaded.output_topology.is_empty(),
            "VST2 reports no speaker placement, so neither side may claim one"
        );
    }

    /// The descriptor carries the plugin's own name, not the file's.
    #[test]
    fn the_descriptor_reports_the_plugins_declared_name() {
        let (p, _lock) = load_probe(&[("TUTTI_VST2_PROBE_EFFECT_NAME", "tutti-probe-named")]);
        assert_eq!(p.descriptor().name, "tutti-probe-named");
    }

    /// A missing file fails at `Opening` rather than panicking.
    ///
    /// The error path has no probe involved, so it is the one case that would
    /// still run if the reference cdylib were missing.
    #[test]
    fn a_missing_file_reports_a_load_failure() {
        let _lock = crate::test_utils::plugin_load_lock();
        // `match` rather than `expect_err`: the `Ok` type is not `Debug`.
        match Vst2Instance::load(
            Path::new("/nonexistent/tutti-not-a-plugin.so"),
            48_000.0,
            512,
        ) {
            Err(BridgeError::LoadFailed { .. }) => {}
            Err(other) => panic!("expected LoadFailed, got {other:?}"),
            Ok(_) => panic!("loading a path that does not exist must fail"),
        }
    }
}
