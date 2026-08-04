//! Audio Unit plugin loader — thin wrapper around `au-host` crate.
//!
//! Follows the same pattern as `vst3_loader.rs` and `clap_loader.rs`.

use std::path::Path;
use tutti_plugin::server::{
    AuComponentType, EditorPresence, Features, LoadedPlugin, PluginClass, PluginDescriptor,
};
#[cfg(all(target_os = "macos", feature = "au"))]
use tutti_plugin::server::{
    EditorSize, ParamAddress, ParamFlags, ParamRange, ParamSteps, ParameterInfo, PluginAudio,
    PluginEditorHost, PluginMeta, PluginParams, PluginResult, PluginState, PluginTail,
    ProcessContext, ProcessOutput, RenderMode, WindowHandle,
};

use crate::loaders::common::{single_bus, Meta};
use tutti_plugin::{BridgeError, LoadStage, Result};

#[cfg(all(target_os = "macos", feature = "au"))]
use tutti_au_host::{
    component, editor::AuEditor, instance::AuInstance as AuHostInstance, parameters, Samples,
};

/// Map the AU host's native component type to the wire `AuComponentType` mirror.
#[cfg(all(target_os = "macos", feature = "au"))]
fn map_au_type(t: tutti_au_host::component::AuType) -> AuComponentType {
    use tutti_au_host::component::AuType;
    match t {
        AuType::Effect => AuComponentType::Effect,
        AuType::Instrument => AuComponentType::Instrument,
        AuType::Generator => AuComponentType::Generator,
        AuType::MusicEffect => AuComponentType::MusicEffect,
        AuType::Mixer => AuComponentType::Mixer,
        AuType::Converter => AuComponentType::Converter,
        AuType::Output => AuComponentType::Output,
        AuType::MidiProcessor => AuComponentType::MidiProcessor,
        AuType::Unknown(code) => AuComponentType::Unknown(code),
    }
}

pub struct AuInstance {
    #[cfg(all(target_os = "macos", feature = "au"))]
    inner: AuHostInstance,
    #[cfg(all(target_os = "macos", feature = "au"))]
    editor: Option<AuEditor>,
    /// Declared `[min, max]` per parameter id, captured once at load.
    ///
    /// `ProcessContext::param_changes` carries **normalized** `0..=1` values (the
    /// host's authoring convention — see `PluginParams::get_parameter`), but AU's
    /// `AudioUnitSetParameter` takes **native plain units**. Denormalizing needs
    /// the declared range, and re-reading `kAudioUnitProperty_ParameterInfo` per
    /// automation point on the audio thread would be a property round-trip per
    /// block, so the ranges are cached here at load time.
    ///
    /// A `Vec` sorted by id rather than a `HashMap`: AU parameter counts are in
    /// the tens, so a binary search beats hashing and keeps the RT path
    /// allocation-free.
    #[cfg(all(target_os = "macos", feature = "au"))]
    param_ranges: Vec<(u32, ParamBounds)>,
    meta: Meta,
}

/// Declared plain-unit bounds for one AU parameter, as reported by
/// `kAudioUnitProperty_ParameterInfo`.
///
/// Raw `f32` Hz/dB/percent/seconds, not `tutti_types` unit newtypes: which
/// physical unit these bounds are in is per-parameter and only known at runtime
/// from `AudioUnitParameterInfo::unit`, and the values cross the AudioToolbox C
/// ABI verbatim.
#[cfg(all(target_os = "macos", feature = "au"))]
#[derive(Debug, Clone, Copy)]
struct ParamBounds {
    min: f32,
    max: f32,
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl ParamBounds {
    /// Map a normalized `0..=1` value onto `[min, max]`.
    ///
    /// Mirrors `tutti_plugin_types::ParameterInfo::to_plain` — the same linear
    /// endpoint map, applied here in `f32` because that is what
    /// `AudioUnitSetParameter` takes.
    ///
    /// This is the *live* path: the return value goes straight into
    /// `AudioUnitSetParameter` on a running unit, so it must never be non-finite.
    /// `normalized` arrives over IPC and the bounds come from the plugin's own
    /// `kAudioUnitProperty_ParameterInfo`, so neither is trusted. NaN needs an
    /// explicit check rather than a clamp: `f32::clamp` returns NaN for NaN, and
    /// `max <= min` is `false` when either is NaN. An infinite *value* still clamps
    /// to an endpoint; an infinite *bound* has no endpoint to clamp to. See
    /// `ParameterInfo::to_plain` for the full argument.
    fn to_plain(self, normalized: f64) -> f32 {
        if !(self.min.is_finite() && self.max.is_finite()) {
            return 0.0;
        }
        if self.max <= self.min {
            return self.min;
        }
        let n = if normalized.is_nan() {
            0.0
        } else {
            normalized.clamp(0.0, 1.0) as f32
        };
        self.min + n * (self.max - self.min)
    }

    /// Inverse of [`to_plain`](Self::to_plain): map the AU's plain value back
    /// onto normalized `0..=1`.
    ///
    /// The read direction of the same boundary. `PluginParams::get_parameter`
    /// is normalized for every format, and `AudioUnitGetParameter` answers in
    /// plain units, so a read without this reports a cutoff of `22050` where
    /// the caller expects `1.0`.
    ///
    /// Same non-finite argument as `to_plain`, in the same order: bounds that
    /// are not finite have no span to divide by, and a degenerate range has no
    /// position to report — both answer `0.0` rather than a NaN or an infinity.
    fn to_normalized(self, plain: f32) -> f64 {
        if !(self.min.is_finite() && self.max.is_finite()) {
            return 0.0;
        }
        if self.max <= self.min || plain.is_nan() {
            return 0.0;
        }
        f64::from((plain.clamp(self.min, self.max) - self.min) / (self.max - self.min))
    }
}

/// Look up the declared bounds for `id` in a range table sorted by id.
#[cfg(all(target_os = "macos", feature = "au"))]
fn lookup_bounds(table: &[(u32, ParamBounds)], id: u32) -> Option<ParamBounds> {
    table
        .binary_search_by_key(&id, |&(pid, _)| pid)
        .ok()
        .map(|i| table[i].1)
}

/// Read every parameter's declared range off the AU, sorted by id for
/// [`lookup_bounds`].
#[cfg(all(target_os = "macos", feature = "au"))]
fn read_param_ranges(unit: tutti_au_host::types::AudioUnit) -> Vec<(u32, ParamBounds)> {
    let mut table: Vec<(u32, ParamBounds)> = parameters::list(unit)
        .into_iter()
        .map(|p| {
            (
                p.id,
                ParamBounds {
                    min: p.range.min,
                    max: p.range.max,
                },
            )
        })
        .collect();
    table.sort_unstable_by_key(|&(id, _)| id);
    table
}

unsafe impl Send for AuInstance {}

impl AuInstance {
    /// Lightweight probe: read AU component info without instantiation.
    pub fn probe(path: &Path) -> Result<PluginDescriptor> {
        #[cfg(all(target_os = "macos", feature = "au"))]
        {
            let bundle_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            let components = component::enumerate_components();
            let component_info = components
                .iter()
                .find(|c| {
                    c.name.contains(&bundle_name)
                        || c.name.ends_with(&bundle_name)
                        || c.name.split(": ").last().is_some_and(|n| n == bundle_name)
                })
                .ok_or_else(|| BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Scanning,
                    reason: format!("No Audio Unit component matching '{}'", bundle_name),
                })?;

            Ok(PluginDescriptor {
                id: format!(
                    "au.{}.{}",
                    tutti_au_host::types::fourcc_to_string(component_info.manufacturer_code),
                    tutti_au_host::types::fourcc_to_string(component_info.sub_type),
                ),
                name: component_info.name.clone(),
                vendor: component_info.manufacturer.clone(),
                version: component_info.version.clone(),
                class: PluginClass::Au {
                    component_type: map_au_type(component_info.component_type),
                },
                // A probe reads the registry entry without instantiating, and
                // `AuEditor::has_editor` needs a live unit. The load path asks.
                editor: EditorPresence::Unknown,
            })
        }
        #[cfg(not(all(target_os = "macos", feature = "au")))]
        Err(BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: "AU support not compiled".to_string(),
        })
    }

    /// Load an Audio Unit from a `.component` bundle path.
    ///
    /// AU plugins are registered system-wide; the path is used for identification
    /// but the actual loading goes through AudioComponentFindNext.
    pub fn load(path: &Path, sample_rate: f64, block_size: usize) -> Result<Self> {
        #[cfg(all(target_os = "macos", feature = "au"))]
        {
            // Strategy: enumerate all components, find one whose name matches
            // the bundle's file stem, or scan by known bundle structure.
            let bundle_name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            // Try to find a matching AU component by name
            let components = component::enumerate_components();
            let matching = components.iter().find(|c| {
                // AU names are typically "Manufacturer: PluginName"
                // Match if the name contains our bundle name
                c.name.contains(&bundle_name)
                    || c.name.ends_with(&bundle_name)
                    // Also try exact match on the part after ": "
                    || c.name
                        .split(": ")
                        .last()
                        .is_some_and(|n| n == bundle_name)
            });

            let component_handle = if let Some(info) = matching {
                info.component
            } else {
                // Fallback: try all effect and instrument types
                return Err(BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Scanning,
                    reason: format!(
                        "No Audio Unit component found matching '{}'. \
                         Available AUs: {}",
                        bundle_name,
                        components
                            .iter()
                            .take(10)
                            .map(|c| c.name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                });
            };

            let component_info = matching.unwrap();

            // Safety: component_handle was obtained from AudioComponentFindNext
            let mut inner =
                unsafe { AuHostInstance::new(component_handle, sample_rate, block_size as u32) }
                    .map_err(|e| BridgeError::LoadFailed {
                        path: path.to_path_buf(),
                        stage: LoadStage::Instantiation,
                        reason: e.to_string(),
                    })?;

            inner.initialize().map_err(|e| BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Initialization,
                reason: e.to_string(),
            })?;

            let name = inner.get_name().unwrap_or_else(|_| bundle_name.clone());
            let has_editor = AuEditor::has_editor(inner.raw_unit());
            // A refusal is compensated as zero rather than failing the load: an
            // AU that will not say how far it delays audio is still a usable
            // plugin, and under-compensating it costs alignment, not audio.
            // This is now a decision on a reachable `Err` — `get_latency` used
            // to swallow the refusal internally, so this arm never ran.
            //
            // No AU registered on macOS 15.6 takes this path (29 of 29 answer),
            // so it is the third-party case, unmeasured by construction.
            let latency = inner.get_latency().unwrap_or(Samples::ZERO);

            // AU is the one format that reports tail in *seconds*, and the one
            // where a refusal is meaningful: every Apple instrument, mixer and
            // generator rejects `kAudioUnitProperty_TailTime` outright, which is
            // "no tail concept", not "no tail". That is `Unknown`.
            //
            // The infinite case is why `PluginTail` has an `Unbounded` arm at
            // all. TAL Reverb 4 answers `f64::INFINITY`, and `Seconds::to_samples`
            // maps every non-finite input to `Samples::ZERO` — deliberately, since
            // that is right for NaN and negatives. Converting first would make an
            // infinite reverb indistinguishable from a plugin with no tail, and a
            // bounce sizing its render from that number truncates the reverb
            // entirely. So the finiteness question is asked *before* the
            // conversion, never after.
            let tail = match inner.get_tail_time() {
                Err(_) => PluginTail::Unknown,
                Ok(seconds) if !seconds.get().is_finite() => PluginTail::Unbounded,
                Ok(seconds) => match seconds.to_samples_ceil(sample_rate) {
                    // A declared-but-zero tail is a real answer: the unit has a
                    // tail concept and says it has none.
                    s if s == Samples::ZERO => PluginTail::None,
                    // `_ceil`, not `_floor`: a render that rounds a tail down
                    // clips its last partial block.
                    s => PluginTail::Finite(s),
                },
            };

            let descriptor = PluginDescriptor {
                id: format!(
                    "au.{}.{}",
                    tutti_au_host::types::fourcc_to_string(component_info.manufacturer_code),
                    tutti_au_host::types::fourcc_to_string(component_info.sub_type),
                ),
                name,
                vendor: component_info.manufacturer.clone(),
                version: component_info.version.clone(),
                class: PluginClass::Au {
                    component_type: map_au_type(component_info.component_type),
                },
                editor: EditorPresence::measured(has_editor),
            };
            // This AUv2 host is f32-only, single-bus, with a Cocoa editor,
            // latency read-back and MIDI *input*. MIDI output, transport/host-
            // callbacks, sample-accurate automation, note-expression, sequencer
            // context, f64, and host-driven editor resize are not implemented
            // (latency presence is derived from `latency_samples`).
            //
            // MIDI output is the one gap that is a wiring job rather than an
            // absent API: `AuInstance::install_midi_output` exists, but nothing
            // here plumbs a callback to it, so the bit stays unprobed.
            let mut features = Features::empty();
            features.set(Features::EDITOR, has_editor);
            // The process path already routes MIDI to any AU whose component
            // type `receives_midi()` — instruments, music effects, MIDI
            // processors. Reporting the same predicate here keeps one fact from
            // being answered twice: before this, every AU instrument declared no
            // MIDI_IN while being sent MIDI on every block.
            features.set(
                Features::MIDI_IN,
                component_info.component_type.receives_midi(),
            );
            // One property backs both bits: a unit that lists factory presets
            // can be asked to load any of them. An empty list is a genuine
            // "none", not a failed read — `factory_presets` absorbs the
            // OSStatus error several working Apple units return.
            let has_presets = !inner.factory_presets().is_empty();
            features.set(Features::PRESET_LIST, has_presets);
            features.set(Features::PRESET_LOAD, has_presets);
            let probed = tutti_plugin::server::probed::AU;

            // AU exposes a single main bus per direction here.
            let loaded = LoadedPlugin {
                // `num_inputs`/`num_outputs` are `u32` off the AU element
                // count; `From<u32>` canonicalizes them, so the `as usize`
                // hop is gone.
                inputs: single_bus(inner.num_inputs()),
                outputs: single_bus(inner.num_outputs()),
                latency_samples: latency,
                tail,
                features,
                probed,
            };

            // Capture the declared plain ranges once, while still on the load
            // thread — the RT path denormalizes against these.
            let param_ranges = read_param_ranges(inner.raw_unit());

            Ok(Self {
                inner,
                editor: None,
                param_ranges,
                meta: Meta { descriptor, loaded },
            })
        }

        #[cfg(not(all(target_os = "macos", feature = "au")))]
        {
            let _ = (path, sample_rate, block_size);
            Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: "Audio Unit support not available (requires macOS + 'au' feature)"
                    .to_string(),
            })
        }
    }
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginMeta for AuInstance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.meta.loaded
    }
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginAudio for AuInstance {
    fn process(
        &mut self,
        buffer: tutti_plugin::server::AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
    ) -> PluginResult<ProcessOutput> {
        // Automation arrives normalized `0..=1` (the host's authoring
        // convention, shared with VST2/VST3), but `AudioUnitSetParameter` takes
        // NATIVE PLAIN UNITS — AU has no normalization concept at all. Writing
        // the normalized value straight through set Apple AUDelay's Lowpass
        // Cutoff (declared `[10, 22050]` Hz) to 1 Hz at full scale and clamped
        // every plain value above 1.0 away, making the entire usable range of
        // every Hz/dB/percent/seconds parameter unreachable.
        //
        // Denormalize against the range the AU itself declared. A parameter
        // missing from the table (the AU grew a parameter after load, or
        // refused `ParameterInfo`) is skipped rather than written blind: a
        // guessed range would be the same class of bug.
        if let Some(changes) = ctx.param_changes {
            for queue in &changes.queues {
                if let Some(point) = queue.points.last() {
                    // `AudioUnitParameterID` is opaque; a VST2 positional index
                    // addresses nothing here.
                    let Some(id) = queue.param_id.opaque().map(|i| i.get()) else {
                        continue;
                    };
                    if let Some(bounds) = lookup_bounds(&self.param_ranges, id) {
                        let _ = self.inner.set_parameter(id, bounds.to_plain(point.value));
                    }
                }
            }
        }

        // Deliver MIDI to instrument / music-effect AUs before rendering, so
        // note-ons scheduled this block sound in it. Plain effects don't
        // consume MIDI (`receives_midi()` is false) — skip the decode for them.
        if !ctx.midi_events.is_empty() && self.inner.au_type().receives_midi() {
            self.inner.send_midi(ctx.midi_events);
        }

        match buffer {
            tutti_plugin::server::AudioBufferMut::F32(buf) => {
                self.inner
                    .process(buf.inputs, buf.outputs, buf.num_samples as u32)
                    .map_err(|e| BridgeError::ProcessError(format!("[au] {e}")))?;
            }
            tutti_plugin::server::AudioBufferMut::F64(buf) => {
                // AUv2 doesn't support f64 natively. Convert f32 -> process -> convert back.
                let input_f32: Vec<Vec<f32>> = buf
                    .inputs
                    .iter()
                    .map(|ch| ch.iter().map(|&s| s as f32).collect())
                    .collect();
                let mut output_f32: Vec<Vec<f32>> = buf
                    .outputs
                    .iter()
                    .map(|ch| vec![0.0f32; ch.len()])
                    .collect();

                let in_slices: Vec<&[f32]> = input_f32.iter().map(|v| v.as_slice()).collect();
                let mut out_slices: Vec<&mut [f32]> =
                    output_f32.iter_mut().map(|v| v.as_mut_slice()).collect();

                self.inner
                    .process(&in_slices, &mut out_slices, buf.num_samples as u32)
                    .map_err(|e| BridgeError::ProcessError(format!("[au] {e}")))?;

                for (ch, out_ch) in buf.outputs.iter_mut().enumerate() {
                    if ch < output_f32.len() {
                        for (i, s) in out_ch.iter_mut().enumerate() {
                            if i < output_f32[ch].len() {
                                *s = output_f32[ch][i] as f64;
                            }
                        }
                    }
                }
            }
        }

        Ok(ProcessOutput::default())
    }

    fn set_sample_rate(&mut self, rate: f64) {
        let _ = self.inner.set_sample_rate(rate);
    }

    /// Write `kAudioUnitProperty_OfflineRender`, bracketed by an
    /// uninitialize/re-initialize cycle.
    ///
    /// The bracket is not optional: a unit that sizes an oversampling or
    /// look-ahead buffer from this flag can only do so at
    /// `AudioUnitInitialize`, so writing it to a live unit is accepted and then
    /// has no effect.
    ///
    /// Returns whether *this unit* accepted the property — the AU half of the
    /// live probe behind [`Features::RENDER_MODE`]. A re-initialization failure
    /// also reports `false`: the caller asked for a mode and did not get it,
    /// and that is the question this bool answers.
    fn set_render_mode(&mut self, mode: RenderMode) -> bool {
        self.inner
            .set_offline_render_bracketed(mode.is_offline())
            .unwrap_or(false)
    }
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginParams for AuInstance {
    /// Normalized `0..=1`, per the [`PluginParams`] contract — `AudioUnitGet`
    /// answers in plain units, so the declared range maps it back.
    ///
    /// Uses the same `param_ranges` table the automation path denormalizes
    /// against, so the direct path and `process` cannot disagree about what a
    /// parameter's bounds are.
    ///
    /// A parameter absent from the table has no declared range to normalize
    /// against; its plain value is reported unchanged rather than scaled by a
    /// guess. `read_param_ranges` lists every parameter the AU declares, so an
    /// absence means the AU did not declare it.
    fn get_parameter(&self, id: ParamAddress) -> f64 {
        // A VST2 index addresses nothing here; `AudioUnitParameterID` is opaque.
        let Some(id) = id.opaque() else { return 0.0 };
        let Ok(plain) = parameters::get(self.inner.raw_unit(), id.get()) else {
            return 0.0;
        };
        match lookup_bounds(&self.param_ranges, id.get()) {
            Some(bounds) => bounds.to_normalized(plain),
            None => f64::from(plain),
        }
    }

    /// Normalized `0..=1` in, matching [`get_parameter`](Self::get_parameter) —
    /// so this pair round-trips. `AudioUnitSetParameter` takes plain units, so
    /// the value is denormalized against the same table.
    fn set_parameter(&mut self, id: ParamAddress, value: f64) {
        let Some(id) = id.opaque() else { return };
        let plain = match lookup_bounds(&self.param_ranges, id.get()) {
            Some(bounds) => bounds.to_plain(value),
            None => value as f32,
        };
        let _ = parameters::set(self.inner.raw_unit(), id.get(), plain);
    }

    fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        parameters::list(self.inner.raw_unit())
            .into_iter()
            .map(|p| {
                // Indexed params are a choice list whose `[min, max]` are the
                // first and last index, so the position count comes from the
                // span — AUTimePitch's "Overlap" is 0..10, eleven positions.
                let steps = match p.unit {
                    parameters::ParameterUnit::Boolean => ParamSteps::Toggle,
                    parameters::ParameterUnit::Indexed => {
                        ParamSteps::from_span((p.range.max - p.range.min) as f64)
                    }
                    _ => ParamSteps::Continuous,
                };
                // AUv2 advertises IsWritable and nothing else. Writability is
                // not automatability — a host may write a parameter the plugin
                // never meant to be automated — so only READ_ONLY is known.
                ParameterInfo {
                    // `AudioUnitParameterID` — AU's opaque plugin-chosen handle,
                    // the same concept as VST3's `ParamID` and CLAP's `clap_id`.
                    id: ParamAddress::Opaque(p.id.into()),
                    name: p.name,
                    unit: p.unit.to_string(),
                    range: ParamRange::Plain {
                        min: p.range.min as f64,
                        max: p.range.max as f64,
                        default: p.range.default as f64,
                    },
                    steps,
                    flags: if p.writable {
                        ParamFlags::empty()
                    } else {
                        ParamFlags::READ_ONLY
                    },
                    known: ParamFlags::READ_ONLY,
                }
            })
            .collect()
    }
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginEditorHost for AuInstance {
    fn open_editor(&mut self, parent: WindowHandle) -> PluginResult<EditorSize> {
        let parent_handle = unsafe { tutti_au_host::WindowHandle::from_raw(parent.as_ptr()) };
        let editor = unsafe { AuEditor::open(self.inner.raw_unit(), Some(parent_handle)) }
            .map_err(|e| BridgeError::EditorError(e.to_string()))?;
        let size = editor.editor_size();
        self.editor = Some(editor);
        Ok(EditorSize {
            width: size.width,
            height: size.height,
        })
    }

    fn close_editor(&mut self) {
        if let Some(mut ed) = self.editor.take() {
            ed.close();
        }
    }
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginState for AuInstance {
    fn get_state(&mut self) -> PluginResult<Vec<u8>> {
        self.inner
            .save_state()
            .map_err(|e| BridgeError::StateSaveError(e.to_string()).into())
    }

    fn set_state(&mut self, data: &[u8]) -> PluginResult<()> {
        self.inner
            .load_state(data)
            .map_err(|e| BridgeError::StateRestoreError(e.to_string()).into())
    }
}

#[cfg(test)]
#[cfg(all(target_os = "macos", feature = "au"))]
mod tests {
    use super::*;

    // Apple's built-in AUDelay should always be available on macOS.
    // Note: AU loading by path requires the component name to match the bundle name.
    // For system AUs, they live in /System/Library/Components/ or
    // /Library/Audio/Plug-Ins/Components/.

    /// An AU that declines `kAudioUnitProperty_TailTime` is `Unknown`, and one
    /// that reports an unbounded tail is `Unbounded` — never `None`, which is
    /// what a plain sample count would have collapsed both to.
    ///
    /// The Apple corpus covers the first half: every Apple instrument, mixer
    /// and generator rejects the property, and every Apple effect answers it.
    /// The second half needs a plugin that reports `f64::INFINITY` — TAL
    /// Reverb 4 does, and no Apple unit exceeds ~21 s — so it runs only when
    /// that unit is installed and says so when it is skipped.
    #[test]
    fn a_declined_tail_is_unknown_and_an_infinite_one_is_unbounded() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::{K_AUDIO_UNIT_TYPE_EFFECT, K_AUDIO_UNIT_TYPE_MUSIC_DEVICE};

        // An effect answers the property, so it must not be `Unknown`.
        let effect = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = component::find_component(&effect).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44_100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let tail = match inner.get_tail_time() {
            Err(_) => PluginTail::Unknown,
            Ok(sec) if !sec.get().is_finite() => PluginTail::Unbounded,
            Ok(sec) => match sec.to_samples_ceil(44_100.0) {
                s if s == Samples::ZERO => PluginTail::None,
                s => PluginTail::Finite(s),
            },
        };
        assert_ne!(
            tail,
            PluginTail::Unknown,
            "AUDelay answers kAudioUnitProperty_TailTime, so its tail is known"
        );

        // An instrument rejects the property outright — that is "no tail
        // concept", which must read as `Unknown` rather than as a zero tail.
        let instrument = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_MUSIC_DEVICE,
            componentSubType: u32::from_be_bytes(*b"dls "),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        if let Some(comp) = component::find_component(&instrument) {
            let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44_100.0, 512) }
                .expect("Should create instance");
            inner.initialize().expect("Should initialize");
            let mapped = match inner.get_tail_time() {
                Err(_) => PluginTail::Unknown,
                Ok(sec) if !sec.get().is_finite() => PluginTail::Unbounded,
                Ok(sec) => match sec.to_samples_ceil(44_100.0) {
                    s if s == Samples::ZERO => PluginTail::None,
                    s => PluginTail::Finite(s),
                },
            };
            // Whatever this unit answers, a *refusal* must never surface as a
            // zero tail — that is the mapping this test exists for.
            if inner.get_tail_time().is_err() {
                assert_eq!(
                    mapped,
                    PluginTail::Unknown,
                    "a refused tail property must be Unknown, not None"
                );
                assert_ne!(mapped.samples(), Some(Samples::ZERO));
            }
        }

        // The unbounded half. TAL Reverb 4 reports `f64::INFINITY`; when it is
        // not installed this leg is skipped, and says so rather than passing
        // silently — a skipped assertion is not a satisfied one.
        let infinite = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"reV4"),
            componentManufacturer: u32::from_be_bytes(*b"TOGU"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        match component::find_component(&infinite) {
            None => {
                eprintln!("SKIP: TAL Reverb 4 not installed; the Unbounded arm is unexercised here")
            }
            Some(comp) => {
                let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44_100.0, 512) }
                    .expect("Should create instance");
                inner.initialize().expect("Should initialize");
                let seconds = inner
                    .get_tail_time()
                    .expect("TAL Reverb 4 answers the tail property");
                assert!(
                    !seconds.get().is_finite(),
                    "TAL Reverb 4 is the corpus's infinite-tail unit; it reported {seconds:?}"
                );

                let mapped = match () {
                    _ if !seconds.get().is_finite() => PluginTail::Unbounded,
                    _ => match seconds.to_samples_ceil(44_100.0) {
                        s if s == Samples::ZERO => PluginTail::None,
                        s => PluginTail::Finite(s),
                    },
                };
                assert_eq!(mapped, PluginTail::Unbounded);
                // The point of the whole type: converting first would have made
                // this `None`, and a bounce would truncate the reverb entirely.
                assert_ne!(mapped, PluginTail::None);
                assert_eq!(seconds.to_samples_ceil(44_100.0), Samples::ZERO);
            }
        }
    }

    #[test]
    fn test_au_enumerate_and_load() {
        use tutti_au_host::component;

        let effects =
            component::enumerate_components_of_type(tutti_au_host::component::AuType::Effect);
        assert!(
            !effects.is_empty(),
            "Should find at least one AU effect on macOS"
        );

        eprintln!("Found {} AU effects", effects.len());
        for (i, info) in effects.iter().take(5).enumerate() {
            eprintln!("  [{}] {} ({})", i, info.name, info.manufacturer);
        }
    }

    #[test]
    fn test_au_parameter_list_via_trait() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;

        // Use Apple's AUDelay directly
        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let param_ranges = read_param_ranges(inner.raw_unit());

        let au = AuInstance {
            inner,
            editor: None,
            param_ranges,
            meta: Meta::default(),
        };

        let params = au.get_parameter_list();
        assert!(
            !params.is_empty(),
            "AUDelay should have parameters via PluginInstance trait"
        );
    }

    #[test]
    fn test_au_process_via_trait() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;
        use tutti_plugin::server::{AudioBuffer as TuttiAudioBuffer, AudioBufferMut};

        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let param_ranges = read_param_ranges(inner.raw_unit());

        let mut au = AuInstance {
            inner,
            editor: None,
            param_ranges,
            meta: Meta::default(),
        };

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = TuttiAudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new();
        let _output = au.process(AudioBufferMut::F32(buffer), &ctx);
        // No crash is the assertion
    }

    /// The live half of the direct path: `PluginParams` is normalized for every
    /// format, so a write of `1.0` must reach AUDelay's cutoff as 22050 Hz and
    /// read back as `1.0` — not as the 1 Hz that writing the normalized value
    /// straight through would set.
    ///
    /// Goes through the trait rather than `ParamBounds` so it covers the
    /// range-table lookup too: a `get`/`set` pair that agreed with each other
    /// but used the wrong bounds would pass a pure-unit test and fail here.
    #[test]
    fn direct_parameter_path_is_normalized_end_to_end() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;

        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let param_ranges = read_param_ranges(inner.raw_unit());

        // The cutoff parameter is the one whose range makes the bug audible.
        let &(cutoff_id, bounds) = param_ranges
            .iter()
            .find(|(_, b)| b.min >= 10.0 && b.max >= 20_000.0)
            .expect("AUDelay declares a wide-range cutoff");

        let mut au = AuInstance {
            inner,
            editor: None,
            param_ranges,
            meta: Meta::default(),
        };
        let addr = ParamAddress::Opaque(cutoff_id.into());

        au.set_parameter(addr, 1.0);
        let plain = parameters::get(au.inner.raw_unit(), cutoff_id).expect("cutoff is readable");
        assert!(
            (plain - bounds.max).abs() < 1.0,
            "normalized 1.0 must set the top of {:?}, got {plain}",
            (bounds.min, bounds.max)
        );
        assert!(
            plain > 2.0,
            "a plain {plain} means the normalized value went through unscaled — \
             the inaudible-filter bug"
        );
        assert!((au.get_parameter(addr) - 1.0).abs() < 1e-3);

        au.set_parameter(addr, 0.0);
        assert!((au.get_parameter(addr)).abs() < 1e-3);
    }

    #[test]
    fn test_au_state_roundtrip_via_trait() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;

        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let param_ranges = read_param_ranges(inner.raw_unit());

        let mut au = AuInstance {
            inner,
            editor: None,
            param_ranges,
            meta: Meta::default(),
        };

        let state = au.get_state().expect("save should succeed");
        assert!(!state.is_empty(), "State should not be empty");

        au.set_state(&state).expect("restore should succeed");
    }

    /// Pure unit half: the normalized→plain map itself.
    ///
    /// The bug was writing the normalized value straight through, which is
    /// equivalent to `to_plain` being the identity. These endpoints are exactly
    /// where identity and the correct map differ, and they use AUDelay's real
    /// declared ranges.
    #[test]
    fn to_plain_maps_onto_the_declared_range_not_identity() {
        let cutoff = ParamBounds {
            min: 10.0,
            max: 22_050.0,
        };
        assert_eq!(cutoff.to_plain(0.0), 10.0);
        assert_eq!(cutoff.to_plain(1.0), 22_050.0);
        assert_eq!(cutoff.to_plain(0.5), 11_030.0);
        // The old code sent 1.0 here — 1 Hz, an inaudible filter.
        assert_ne!(cutoff.to_plain(1.0), 1.0);

        // Negative minima (AUDelay Feedback is [-99.9, 99.9]) must map too; the
        // old clamp to [0,1] made the entire negative half unreachable.
        let feedback = ParamBounds {
            min: -99.9,
            max: 99.9,
        };
        assert!((feedback.to_plain(0.0) - -99.9).abs() < 1e-3);
        assert!(feedback.to_plain(0.5).abs() < 1e-3);
        assert!((feedback.to_plain(1.0) - 99.9).abs() < 1e-3);

        // Out-of-range input is clamped to the declared endpoints, never past.
        assert_eq!(cutoff.to_plain(-5.0), 10.0);
        assert_eq!(cutoff.to_plain(9.0), 22_050.0);

        // A degenerate range yields `min` rather than NaN/inf.
        let degenerate = ParamBounds { min: 3.0, max: 3.0 };
        assert_eq!(degenerate.to_plain(0.5), 3.0);
    }

    /// The live-path half: this `to_plain`'s return value goes straight into
    /// `AudioUnitSetParameter` on a running unit, so a NaN escaping here is a NaN
    /// in a live filter coefficient.
    ///
    /// `normalized` comes over IPC and the bounds come from the plugin's own
    /// `kAudioUnitProperty_ParameterInfo`, so neither is trusted. Clamping does not
    /// substitute for the check: `f32::clamp` returns NaN for NaN, and `max <= min`
    /// is `false` when either bound is NaN.
    #[test]
    fn nan_never_reaches_a_live_au_parameter() {
        let cutoff = ParamBounds {
            min: 10.0,
            max: 22_050.0,
        };

        assert!(
            cutoff.to_plain(f64::NAN).is_finite(),
            "a NaN automation point must not reach AudioUnitSetParameter"
        );
        assert_eq!(cutoff.to_plain(f64::INFINITY), 22_050.0);
        assert_eq!(cutoff.to_plain(f64::NEG_INFINITY), 10.0);

        // Bounds the AU itself reported as non-finite.
        for (min, max) in [
            (f32::NAN, 1.0),
            (0.0, f32::NAN),
            (f32::NEG_INFINITY, 1.0),
            (0.0, f32::INFINITY),
        ] {
            let broken = ParamBounds { min, max };
            for v in [0.0, 0.5, 1.0, f64::NAN] {
                assert!(
                    broken.to_plain(v).is_finite(),
                    "to_plain({v}) with bounds [{min}, {max}] returned non-finite"
                );
            }
        }
    }

    #[test]
    fn lookup_bounds_finds_ids_in_a_sorted_table() {
        let table = vec![
            (2u32, ParamBounds { min: 0.0, max: 1.0 }),
            (
                7u32,
                ParamBounds {
                    min: 10.0,
                    max: 22_050.0,
                },
            ),
        ];
        assert_eq!(lookup_bounds(&table, 7).map(|b| b.max), Some(22_050.0));
        assert_eq!(lookup_bounds(&table, 2).map(|b| b.min), Some(0.0));
        // A parameter the AU never declared has no range to denormalize
        // against, so it must be reported missing (and skipped), not guessed.
        assert!(lookup_bounds(&table, 3).is_none());
    }

    /// Live half: drive a real AU's automation path, assert the unit holds the
    /// native value, then assert the read path inverts it.
    ///
    /// Full-scale automation must land on the parameter's declared MAXIMUM, not
    /// on `1.0` — that is the denormalization. And `get_parameter` must answer
    /// `1.0` rather than the maximum — that is the `PluginParams` contract,
    /// which is normalized for every format. Both directions in one test
    /// because either alone can pass while the pair is inconsistent.
    #[test]
    fn param_automation_denormalizes_and_reads_back_normalized() {
        use tutti_au_host::component;
        use tutti_au_host::types::AudioComponentDescription;
        use tutti_au_host::types::K_AUDIO_UNIT_TYPE_EFFECT;
        use tutti_plugin::server::{
            AudioBuffer as TuttiAudioBuffer, AudioBufferMut, ParameterChanges, ParameterQueue,
        };

        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };

        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");
        let param_ranges = read_param_ranges(inner.raw_unit());

        let mut au = AuInstance {
            inner,
            editor: None,
            param_ranges,
            meta: Meta::default(),
        };

        // Pick a writable parameter with a genuinely wide plain range — one
        // whose max is far from 1.0, so identity-vs-denormalized is decidable.
        let target = au
            .get_parameter_list()
            .into_iter()
            .find(|p| {
                p.range.bounds().is_some_and(|(_, max)| max > 2.0)
                    && p.flag(ParamFlags::READ_ONLY) == Some(false)
            })
            .expect("AUDelay should expose a wide-range writable parameter");
        let (min, max) = target
            .range
            .bounds()
            .expect("AU declares a plain range for every parameter");

        let num_samples = 64;
        for (normalized, expected) in [
            (1.0f64, max),
            (0.0f64, min),
            (0.5f64, min + 0.5 * (max - min)),
        ] {
            let mut changes = ParameterChanges::new();
            // The queue is keyed by the same `ParamAddress` the descriptor
            // carries, so nothing here has to restate that AU is opaque.
            let mut queue = ParameterQueue::new(target.id);
            queue.add_point(0, normalized);
            changes.add_queue(queue);

            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];
            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();
            let buffer = TuttiAudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };
            let mut ctx = ProcessContext::new();
            ctx.param_changes = Some(&changes);
            au.process(AudioBufferMut::F32(buffer), &ctx)
                .expect("process should succeed");

            // Tolerance scales with the range: AU stores parameters as f32, so
            // a 22 kHz range round-trips to ~1e-4 relative precision.
            let tolerance = (max - min).abs() * 1e-4;

            // The subject of this test: the automation point was normalized,
            // and the AU must hold the *native* value. Read straight off the
            // unit rather than through `get_parameter`, which now normalizes —
            // going through it would assert the identity of two conversions and
            // pass even if both were wrong.
            let opaque = target.id.opaque().expect("AU ids are opaque").get();
            let native =
                f64::from(parameters::get(au.inner.raw_unit(), opaque).expect("param is readable"));
            assert!(
                (native - expected).abs() <= tolerance,
                "param {} ('{}'): normalized {normalized} should reach the AU as \
                 {expected} in native units, got {native} (range [{min}, {max}])",
                target.id,
                target.name,
            );

            // And the read path inverts it: `PluginParams` is normalized for
            // every format, so what went in comes back out.
            let read_back = au.get_parameter(target.id);
            assert!(
                (read_back - normalized).abs() <= 1e-4,
                "param {} ('{}'): should read back as the normalized {normalized}, \
                 got {read_back}",
                target.id,
                target.name,
            );
        }
    }
}
