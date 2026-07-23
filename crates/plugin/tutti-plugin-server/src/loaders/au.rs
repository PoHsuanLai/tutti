//! Audio Unit plugin loader — thin wrapper around `au-host` crate.
//!
//! Follows the same pattern as `vst3_loader.rs` and `clap_loader.rs`.

#![allow(dead_code)]

use std::path::Path;
use tutti_plugin::server::{
    AuComponentType, Features, LoadedPlugin, PluginClass, PluginDescriptor,
};
#[cfg(all(target_os = "macos", feature = "au"))]
use tutti_plugin::server::{
    EditorSize, ParameterInfo, PluginInstance, PluginResult, ProcessContext, ProcessOutput,
    WindowHandle,
};

#[cfg(all(target_os = "macos", feature = "au"))]
use crate::loaders::common::params::{make_param_info, ALL_AUTOMATABLE};
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
    meta: Meta,
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

            Ok(Self {
                inner,
                editor: None,
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

    pub fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }
}

#[cfg(all(target_os = "macos", feature = "au"))]
impl PluginInstance for AuInstance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.meta.loaded
    }

    fn process(
        &mut self,
        buffer: tutti_plugin::server::AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
    ) -> PluginResult<ProcessOutput> {
        if let Some(changes) = ctx.param_changes {
            for queue in &changes.queues {
                if let Some(point) = queue.points.last() {
                    let _ = self.inner.set_parameter(queue.param_id, point.value as f32);
                }
            }
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

    fn get_parameter(&self, id: u32) -> f64 {
        parameters::get(self.inner.raw_unit(), id).unwrap_or(0.0) as f64
    }

    fn set_parameter(&mut self, id: u32, value: f64) {
        let _ = parameters::set(self.inner.raw_unit(), id, value as f32);
    }

    fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        parameters::list(self.inner.raw_unit())
            .into_iter()
            .map(|p| {
                make_param_info(
                    p.id,
                    p.name,
                    p.unit.to_string(),
                    p.range.min as f64,
                    p.range.max as f64,
                    p.range.default as f64,
                    0,
                    ALL_AUTOMATABLE,
                )
            })
            .collect()
    }

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
    use tutti_plugin::server::PluginInstance;

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
            component_type: K_AUDIO_UNIT_TYPE_EFFECT,
            component_sub_type: u32::from_be_bytes(*b"dely"),
            component_manufacturer: u32::from_be_bytes(*b"appl"),
            component_flags: 0,
            component_flags_mask: 0,
        };

        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");

        let au = AuInstance {
            inner,
            editor: None,
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
            component_type: K_AUDIO_UNIT_TYPE_EFFECT,
            component_sub_type: u32::from_be_bytes(*b"dely"),
            component_manufacturer: u32::from_be_bytes(*b"appl"),
            component_flags: 0,
            component_flags_mask: 0,
        };

        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");

        let mut au = AuInstance {
            inner,
            editor: None,
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
            component_type: K_AUDIO_UNIT_TYPE_EFFECT,
            component_sub_type: u32::from_be_bytes(*b"dely"),
            component_manufacturer: u32::from_be_bytes(*b"appl"),
            component_flags: 0,
            component_flags_mask: 0,
        };

        let comp = component::find_component(&desc).expect("AUDelay should exist");
        let mut inner = unsafe { tutti_au_host::AuInstance::new(comp, 44100.0, 512) }
            .expect("Should create instance");
        inner.initialize().expect("Should initialize");

        let mut au = AuInstance {
            inner,
            editor: None,
            meta: Meta::default(),
        };

        let state = au.get_state().expect("save should succeed");
        assert!(!state.is_empty(), "State should not be empty");

        au.set_state(&state).expect("restore should succeed");
    }
}
