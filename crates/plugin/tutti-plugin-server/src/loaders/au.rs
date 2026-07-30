//! Audio Unit plugin loader — thin wrapper around `au-host` crate.
//!
//! Follows the same pattern as `vst3_loader.rs` and `clap_loader.rs`.

use std::path::Path;
use tutti_plugin::server::{
    AuComponentType, Features, LoadedPlugin, PluginClass, PluginDescriptor,
};
#[cfg(all(target_os = "macos", feature = "au"))]
use tutti_plugin::server::{
    EditorSize, ParamFlags, ParamRange, ParamSteps, ParameterInfo, PluginAudio, PluginEditorHost,
    PluginMeta, PluginParams, PluginResult, PluginState, ProcessContext, ProcessOutput,
    WindowHandle,
};

use crate::loaders::common::{single_bus, Meta};
use tutti_plugin::{BridgeError, LoadStage, Result};

#[cfg(all(target_os = "macos", feature = "au"))]
use tutti_au_host::{
    component, editor::AuEditor, instance::AuInstance as AuHostInstance, parameters,
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
                version: String::new(),
                class: PluginClass::Au {
                    component_type: map_au_type(component_info.component_type),
                },
                has_editor: false,
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
            let latency = inner.get_latency().unwrap_or(0) as usize;

            let descriptor = PluginDescriptor {
                id: format!(
                    "au.{}.{}",
                    tutti_au_host::types::fourcc_to_string(component_info.manufacturer_code),
                    tutti_au_host::types::fourcc_to_string(component_info.sub_type),
                ),
                name,
                vendor: component_info.manufacturer.clone(),
                version: String::new(),
                class: PluginClass::Au {
                    component_type: map_au_type(component_info.component_type),
                },
                has_editor,
            };
            // This AUv2 host is f32-only, single-bus, with a Cocoa editor and
            // latency read-back. MIDI I/O, transport/host-callbacks, sample-
            // accurate automation, note-expression, sequencer context, f64, and
            // host-driven editor resize are not implemented — so only EDITOR is
            // set (latency presence is derived from `latency_samples`).
            let mut features = Features::empty();
            features.set(Features::EDITOR, has_editor);

            // AU exposes a single main bus per direction here.
            let loaded = LoadedPlugin {
                inputs: single_bus(inner.num_inputs() as usize),
                outputs: single_bus(inner.num_outputs() as usize),
                latency_samples: latency,
                features,
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
                    if let Some(bounds) = lookup_bounds(&self.param_ranges, queue.param_id) {
                        let _ = self
                            .inner
                            .set_parameter(queue.param_id, bounds.to_plain(point.value));
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
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginParams for AuInstance {
    /// Plain native units, per the [`PluginParams`] contract for AU — pass the
    /// AU's value through unchanged.
    fn get_parameter(&self, id: u32) -> f64 {
        parameters::get(self.inner.raw_unit(), id).unwrap_or(0.0) as f64
    }

    /// Plain native units in, matching [`get_parameter`](Self::get_parameter) —
    /// so this pair round-trips. (The `param_changes` automation path in
    /// `process` is the one that must denormalize, because ITS input is
    /// Normalized; see the note there.)
    fn set_parameter(&mut self, id: u32, value: f64) {
        let _ = parameters::set(self.inner.raw_unit(), id, value as f32);
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
                    id: p.id,
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

    /// Live half: drive a real AU's automation path and read the value
    /// back in native units. Full-scale automation must land on the parameter's
    /// declared MAXIMUM, not on `1.0`.
    #[test]
    fn param_automation_round_trips_in_native_units() {
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

            let read_back = au.get_parameter(target.id);
            // Tolerance scales with the range: AU stores parameters as f32, so
            // a 22 kHz range round-trips to ~1e-4 relative precision.
            let tolerance = (max - min).abs() * 1e-4;
            assert!(
                (read_back - expected).abs() <= tolerance,
                "param {} ('{}'): normalized {normalized} should read back as \
                 {expected} in native units, got {read_back} (range [{min}, {max}])",
                target.id,
                target.name,
            );
        }
    }
}
