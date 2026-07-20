//! High-level VST2 plugin instance.
//!
//! [`Vst2Instance`] is the main entry point. Construct via
//! [`Vst2Instance::load`]; the constructor runs the full VST2 init/resume
//! sequence so the returned instance is immediately usable for
//! [`process_f32`](Self::process_f32) / [`process_f64`](Self::process_f64).
//!
//! Unlike `au-host`'s two-stage `Loaded → Ready` split, VST2's lifecycle
//! is short and atomic — `init → set_sample_rate → set_block_size →
//! resume` all happen at construction. There's no useful state between
//! "ready to load editor / params" and "ready to process audio", so we
//! collapse them.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use vst::host::PluginLoader;
use vst::plugin::{Category, Plugin as _};

use crate::error::{LoadStage, Result, Vst2Error};
use crate::handle::Vst2Handle;
use crate::host::{HostLink, HostState};
use crate::midi::MidiIo;
use crate::parameters::SendParams;
use crate::types::{PluginInfo, Vst2Category};

/// Map the `vst` crate's `Category` to the shared [`Vst2Category`] mirror.
/// A free fn rather than a `From` impl: both `Category` (from `vst`) and
/// `Vst2Category` (from `tutti-plugin-types`) are foreign here, so the orphan
/// rule forbids the impl.
fn map_category(c: Category) -> Vst2Category {
    match c {
        Category::Unknown => Vst2Category::Unknown,
        Category::Effect => Vst2Category::Effect,
        Category::Synth => Vst2Category::Synth,
        Category::Analysis => Vst2Category::Analysis,
        Category::Mastering => Vst2Category::Mastering,
        Category::Spacializer => Vst2Category::Spacializer,
        Category::RoomFx => Vst2Category::RoomFx,
        Category::SurroundFx => Vst2Category::SurroundFx,
        Category::Restoration => Vst2Category::Restoration,
        Category::OfflineProcess => Vst2Category::OfflineProcess,
        Category::Shell => Vst2Category::Shell,
        Category::Generator => Vst2Category::Generator,
    }
}

/// Loaded, initialized, processing-ready VST2 plugin.
///
/// The instance owns the `vst::PluginInstance` (via `Vst2Handle`) plus
/// the channel ends fed by the host-callback `HostState`. `Send` but not
/// `Sync` — callers serialize access (subprocess server is single-threaded;
/// in-process backend uses a `parking_lot::Mutex`).
pub struct Vst2Instance {
    /// The loaded `vst::PluginInstance` (owns the editor handle + teardown).
    pub(crate) handle: Vst2Handle,
    /// The plugin's parameter object (get/set/preset access).
    pub(crate) params: SendParams,
    /// Host-callback channel endpoints + the shared transport snapshot.
    pub(crate) host_link: HostLink,
    /// Per-block MIDI plumbing (host→plugin staging, plugin→host drain).
    pub(crate) midi: MidiIo,
    metadata: PluginInfo,
}

// SAFETY: every field is either `Send` or its non-`Send`-ness has been
// addressed via a wrapper (`SendEditor`, `SendParams`). The raw pointers
// inside `vst::host::PluginInstance` point at heap memory we own;
// callers serialize access externally (subprocess server is single-
// threaded; in-process backend uses `parking_lot::Mutex`). `Sync` is
// needed by the in-process callers — fundsp's `dyn AudioUnit` requires
// `Send + Sync` and the audio unit holds `Arc<Mutex<Vst2Instance>>`.
unsafe impl Send for Vst2Instance {}
unsafe impl Sync for Vst2Instance {}

impl Vst2Instance {
    /// Load a VST2 bundle and run the full init/resume sequence.
    ///
    /// On macOS `.vst` is a bundle directory; this constructor probes
    /// `Contents/MacOS/` for the actual binary. Plain shared-library
    /// paths (Linux `.so`, Windows `.dll`) are used as-is.
    ///
    /// `block_size` is the maximum number of samples per process call;
    /// callers may render fewer per call, but never more.
    pub fn load(path: &Path, sample_rate: f64, block_size: usize) -> Result<Self> {
        let resolved = resolve_bundle(path);

        let (param_tx, param_rx) = crossbeam_channel::unbounded();
        let (midi_out_tx, midi_out_rx) = crossbeam_channel::unbounded();
        let time_info = Arc::new(arc_swap::ArcSwap::from_pointee(None));
        let host = Arc::new(Mutex::new(HostState::new(
            param_tx,
            midi_out_tx,
            Arc::clone(&time_info),
        )));

        let mut loader = PluginLoader::load(&resolved, Arc::clone(&host)).map_err(|e| {
            Vst2Error::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Factory,
                reason: format!("PluginLoader::load failed: {:?}", e),
            }
        })?;

        let mut instance = loader.instance().map_err(|e| Vst2Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Instantiation,
            reason: format!("loader.instance failed: {:?}", e),
        })?;

        instance.init();
        instance.set_sample_rate(sample_rate as f32);
        instance.set_block_size(block_size as i64);
        instance.resume();

        let info = instance.get_info();
        let receives_midi = info.midi_inputs > 0
            || info.midi_outputs > 0
            || matches!(info.category, Category::Synth);
        let metadata = PluginInfo {
            id: format!("vst2.{}", info.unique_id),
            name: info.name.clone(),
            vendor: info.vendor.clone(),
            version: info.version.to_string(),
            num_inputs: info.inputs as usize,
            num_outputs: info.outputs as usize,
            category: map_category(info.category),
            receives_midi,
            emits_midi: info.midi_outputs > 0,
            has_editor: false, // overwritten below once we ask the handle
            latency_samples: info.initial_delay.max(0) as usize,
            supports_f64: info.f64_precision,
        };

        let params = SendParams(instance.get_parameter_object());
        let handle = Vst2Handle::new(instance);
        let mut metadata = metadata;
        metadata.has_editor = handle.has_editor();

        Ok(Self {
            handle,
            params,
            host_link: HostLink {
                _state: host,
                time_info,
                param_rx,
            },
            midi: MidiIo::new(midi_out_rx),
            metadata,
        })
    }

    /// Plugin metadata snapshot captured at load time.
    pub fn metadata(&self) -> &PluginInfo {
        &self.metadata
    }

    /// Notify the plugin of a sample-rate change.
    ///
    /// Note: the `vst` crate doesn't suspend/resume around this call.
    /// Some VST2 plugins assume the host suspends them first; if you hit
    /// glitches or crashes after a rate change, suspend manually before
    /// calling.
    pub fn set_sample_rate(&mut self, sample_rate: f64) {
        self.handle.instance.set_sample_rate(sample_rate as f32);
    }

    /// Notify the plugin of a maximum-block-size change. Like
    /// [`set_sample_rate`](Self::set_sample_rate), the `vst` crate does
    /// not suspend around the call.
    pub fn set_block_size(&mut self, block_size: usize) {
        self.handle.instance.set_block_size(block_size as i64);
    }
}

/// Resolve a `.vst` bundle directory to its inner Mach-O / ELF binary.
///
/// Plain files and nonexistent paths pass through unchanged — the caller
/// surfaces the load failure with its own diagnostic. Bundle layouts:
/// macOS `Contents/MacOS/<stem>`, Linux `Contents/x86_64-linux/<stem>.so`,
/// Windows `Contents/x86_64-win/<stem>.dll`.
pub fn resolve_bundle(path: &Path) -> PathBuf {
    if path.is_file() || !path.is_dir() {
        return path.to_path_buf();
    }

    #[cfg(target_os = "macos")]
    let resolved = probe_subdir(path, "MacOS", None);

    #[cfg(target_os = "linux")]
    let resolved = probe_subdir(path, "x86_64-linux", Some("so"));

    #[cfg(target_os = "windows")]
    let resolved = probe_subdir(path, "x86_64-win", Some("dll"));

    resolved.unwrap_or_else(|| path.to_path_buf())
}

fn probe_subdir(bundle: &Path, arch_dir: &str, ext: Option<&str>) -> Option<PathBuf> {
    let dir = bundle.join("Contents").join(arch_dir);
    let stem = bundle.file_stem()?;

    let candidate = dir.join(stem);
    if candidate.is_file() {
        return Some(candidate);
    }

    if let Some(ext) = ext {
        let with_ext = dir.join(format!("{}.{}", stem.to_str()?, ext));
        if with_ext.is_file() {
            return Some(with_ext);
        }
    }

    if let Some(name) = bundle.file_name() {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }

    None
}
