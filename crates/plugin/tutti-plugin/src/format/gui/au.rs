//! In-process Audio Unit GUI instance (editor only, no audio processing).
//! macOS only.
//!
//! Implements the host-side [`PluginEditor`](super::PluginEditor) trait. Even
//! though `AuInstance` (in `loaders/au.rs`) owns the *subprocess-side* editor
//! open/close/state paths, this host-process object is a separate dlopen: the
//! two editor surfaces stay distinct by design, one per world.

use super::PluginEditor;
use crate::error::{BridgeError, LoadStage, Result};
use crate::protocol::{Normalized, ParamAddress, ParamRange};

/// Look up declared bounds for `id` in a table sorted by id.
fn lookup_range(table: &[(u32, (f64, f64))], id: u32) -> Option<(f64, f64)> {
    table
        .binary_search_by_key(&id, |(k, _)| *k)
        .ok()
        .map(|i| table[i].1)
}
use crate::util::window::{EditorSize, WindowHandle};
use std::path::Path;

pub(crate) struct AuGuiInstance {
    inner: tutti_au_host::AuInstance,
    editor: Option<tutti_au_host::AuEditor>,
    /// Declared plain-unit bounds per parameter id, sorted, captured at load.
    ///
    /// AU takes plain units where this host speaks normalized, so a mirrored
    /// write has to be denormalized — and the range is only knowable by asking
    /// the plugin. Cached rather than queried per write for the reason the
    /// subprocess loader caches the same table: `kAudioUnitProperty_ParameterInfo`
    /// is a property round-trip into the plugin, and this path runs on every
    /// knob movement.
    ///
    /// A `Vec` sorted by id rather than a `HashMap`, matching the loader: AU
    /// parameter counts are in the tens, so a binary search wins.
    param_ranges: Vec<(u32, (f64, f64))>,
}

impl AuGuiInstance {
    pub fn load(path: &Path) -> Result<Self> {
        // AU plugins are system-registered; find matching component by bundle name.
        let bundle_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();

        let components = tutti_au_host::component::enumerate_components();
        let matching = components.iter().find(|c| {
            c.name.contains(&bundle_name)
                || c.name.ends_with(&bundle_name)
                || c.name.split(": ").last().is_some_and(|n| n == bundle_name)
        });

        let component_handle =
            matching
                .map(|info| info.component)
                .ok_or_else(|| BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Scanning,
                    reason: format!("No Audio Unit component found matching '{bundle_name}'"),
                })?;

        // Create instance but do NOT initialize — GUI works without audio setup.
        let inner = unsafe { tutti_au_host::AuInstance::new(component_handle, 44100.0, 512) }
            .map_err(|e| BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Instantiation,
                reason: format!("AU GUI-only load failed: {e}"),
            })?;

        // Readable on an uninitialized unit: this is a property fetch, not a
        // render-time query, which is what makes a GUI-only instance able to
        // build the table at all.
        let mut param_ranges: Vec<(u32, (f64, f64))> =
            tutti_au_host::parameters::list(inner.raw_unit())
                .into_iter()
                .map(|p| (p.id, (p.range.min as f64, p.range.max as f64)))
                .collect();
        param_ranges.sort_unstable_by_key(|(id, _)| *id);

        Ok(Self {
            inner,
            editor: None,
            param_ranges,
        })
    }
}

impl PluginEditor for AuGuiInstance {
    fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        let parent_handle = unsafe { tutti_au_host::WindowHandle::from_raw(parent.as_ptr()) };
        let editor =
            // No size to offer here either: this entry point receives a
            // parent handle and nothing else. See the sibling in
            // `tutti-plugin-server`.
            unsafe {
                tutti_au_host::AuEditor::open(
                    self.inner.raw_unit(),
                    Some(parent_handle),
                    tutti_plugin_types::EditorSize {
                        width: 800,
                        height: 600,
                    },
                )
            }
                .map_err(|e| BridgeError::ProtocolError(format!("AU open_editor failed: {e}")))?;
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

    fn editor_idle(&mut self) {
        // AUv2 Cocoa views are driven by the AppKit run loop; no explicit idle needed.
    }

    fn set_parameter(&mut self, id: ParamAddress, value: Normalized) {
        // `AudioUnitParameterID` is opaque; a VST2 index addresses nothing here.
        let Some(id) = id.opaque() else { return };

        // `AudioUnitSetParameter` takes **plain** units, so the host's
        // normalized value is denormalized against the declared range — the
        // same conversion the subprocess loader applies on the audio path.
        // Without it, mirroring a write to a `[10, 22050]` Hz cutoff moved the
        // editor's knob to 1 Hz while the audio path went to full scale, so
        // the display disagreed with what was heard.
        //
        // An id with no cached range is written through unconverted: the table
        // lists every parameter the AU declared, so a miss means an id this
        // plugin does not have, and `set_parameter` will reject it anyway.
        let plain = match lookup_range(&self.param_ranges, id.get()) {
            Some((min, max)) => ParamRange::Plain {
                min,
                max,
                default: min,
            }
            .to_plain(value.get()) as f32,
            None => value.get() as f32,
        };
        let _ = self.inner.set_parameter(id.get(), plain);
    }

    fn set_state(&mut self, data: &[u8]) -> Result<()> {
        self.inner
            .set_state(data)
            .map_err(|e| BridgeError::ProtocolError(format!("AU set_state failed: {e}")))
    }

    fn poll_gui_param_changes(&mut self) -> Vec<(ParamAddress, f32)> {
        // AUv2 doesn't have a built-in parameter change notification from GUI.
        // Parameter changes from Cocoa views go through AudioUnitSetParameter directly.
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The GUI mirror denormalizes against the declared range.
    ///
    /// This is the bug the type change exposed: `set_parameter_rt` hands the
    /// same normalized value to the audio path *and* to this mirror, but
    /// `AudioUnitSetParameter` takes plain units. Writing `1.0` to AUDelay's
    /// "Lowpass Cutoff" (`[10, 22050]` Hz) moved the editor's knob to 1 Hz
    /// while the audio path went to full scale — the display disagreeing with
    /// what was heard.
    ///
    /// Asserts against the *cached table*, which is what the write path reads,
    /// so a mistake in building it fails here rather than only in a UI.
    #[test]
    fn the_gui_mirror_denormalizes_against_the_declared_range() {
        let Ok(gui) = AuGuiInstance::load(Path::new("AUDelay")) else {
            eprintln!("AUDelay unavailable; skipping");
            return;
        };

        assert!(
            !gui.param_ranges.is_empty(),
            "AUDelay declares parameters, so the cache must not be empty"
        );

        // A parameter whose range is not 0..=1 — otherwise normalized and plain
        // coincide and the test cannot tell a conversion from a pass-through.
        //
        // Asserted, not skipped. AUDelay's "Delay Time" is `[0, 2]` s and
        // "Lowpass Cutoff" `[10, 22050]` Hz (measured, macOS 15.6), so a unit
        // with *no* wide range here means the cache was built wrong — which is
        // exactly the failure this test exists to catch. An `else { return }`
        // would turn that into a silent pass.
        let &(id, (min, max)) = gui
            .param_ranges
            .iter()
            .find(|(_, (min, max))| *min != 0.0 || *max != 1.0)
            .expect("AUDelay declares parameters with ranges wider than 0..=1");

        let range = ParamRange::Plain {
            min,
            max,
            default: min,
        };
        assert_eq!(range.to_plain(Normalized::new(1.0).get()), max);
        assert_eq!(range.to_plain(Normalized::new(0.0).get()), min);
        assert_eq!(lookup_range(&gui.param_ranges, id), Some((min, max)));

        // The table is sorted, which `lookup_range`'s binary search requires.
        assert!(
            gui.param_ranges.windows(2).all(|w| w[0].0 < w[1].0),
            "the range table must be sorted by id and free of duplicates"
        );
    }
}
