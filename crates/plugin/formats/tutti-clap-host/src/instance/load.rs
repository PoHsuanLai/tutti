//! Construction of [`ClapLoaded`]: probe, load, and the editor-only entry
//! point, plus the port-layout / f64-support helpers they read off the freshly
//! instantiated plugin.

use super::config::{AudioConfig, LifecycleFlags, PortLayout};
use super::descriptor::{self, load_descriptor};
use super::ext;
use super::extensions::ExtensionCache;
use super::handle::PluginHandle;
use super::ClapLoaded;
use crate::error::{ClapError, LoadStage, Result};
use crate::host::{ClapHost, HostState};
use crate::types::PluginInfo;
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_SUPPORTS_64BITS,
};
use clap_sys::plugin::clap_plugin;
use std::path::Path;
use std::sync::Arc;

impl ClapLoaded {
    /// Lightweight probe: read the CLAP descriptor without creating or
    /// initializing the plugin instance.
    pub fn probe(bundle_path: &Path, library_path: Option<&Path>) -> Result<PluginInfo> {
        let load_path = library_path.unwrap_or(bundle_path);

        let library = unsafe {
            libloading::Library::new(load_path).map_err(|e| ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: format!("Failed to load library: {e}"),
            })?
        };

        let loaded = load_descriptor(&library, bundle_path)?;
        let info = loaded.info;
        drop(loaded.entry_guard); // drop order: guard before library
        drop(library);
        Ok(info)
    }

    /// Load a CLAP plugin from a path that is either a file or a bundle directory.
    pub fn load(path: impl AsRef<Path>, sample_rate: f64, max_frames: u32) -> Result<Self> {
        Self::load_with_library(path.as_ref(), None, sample_rate, max_frames)
    }

    /// Load a CLAP plugin with a pre-resolved library path.
    ///
    /// `bundle_path` is the original `.clap` bundle directory (passed to `init()`).
    /// `library_path` is the resolved binary for dlopen. If `None`, `bundle_path`
    /// is used for both.
    pub fn load_with_library(
        bundle_path: &Path,
        library_path: Option<&Path>,
        sample_rate: f64,
        max_frames: u32,
    ) -> Result<Self> {
        let load_path = library_path.unwrap_or(bundle_path);

        let library = unsafe {
            libloading::Library::new(load_path).map_err(|e| ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: format!("Failed to load library: {e}"),
            })?
        };

        let descriptor::LoadedDescriptor {
            entry_guard,
            factory_ptr,
            factory,
            info: mut plugin_info,
        } = load_descriptor(&library, bundle_path)?;

        let host_state = Arc::new(HostState::new());
        let host = Box::new(ClapHost::new(host_state.clone()));

        let plugin_id_cstr =
            std::ffi::CString::new(plugin_info.id.as_str()).map_err(|e| ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Instantiation,
                reason: format!("Invalid plugin ID: {e}"),
            })?;

        let create_fn = factory.create_plugin.ok_or_else(|| ClapError::LoadFailed {
            path: bundle_path.to_path_buf(),
            stage: LoadStage::Instantiation,
            reason: "No create_plugin function".to_string(),
        })?;

        let plugin_ptr = unsafe {
            create_fn(
                factory_ptr as *const _,
                host.as_raw(),
                plugin_id_cstr.as_ptr(),
            )
        };

        if plugin_ptr.is_null() {
            return Err(ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Instantiation,
                reason: "Failed to create plugin instance".to_string(),
            });
        }

        // H5: bind the raw pointer into its owning handle BEFORE the init
        // checks. `create_plugin` has already handed us ownership, so every
        // exit from here on must `destroy()` it — CLAP's spec is explicit: "If
        // init returns false, the host must destroy the plugin instance."
        // Previously both early returns below (missing `init`, `init` false)
        // dropped the raw pointer on the floor and then `dlclose`d the library
        // out from under a live instance. `PluginHandle::drop` now covers both.
        let plugin = PluginHandle::new(plugin_ptr);

        let plugin_init_fn = unsafe { plugin.as_ref() }
            .init
            .ok_or_else(|| ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Initialization,
                reason: "No plugin init function".to_string(),
            })?;

        if !unsafe { plugin_init_fn(plugin.as_ptr()) } {
            return Err(ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Initialization,
                reason: "Plugin init failed".to_string(),
            });
        }

        let extensions = ExtensionCache::query(plugin.as_ptr());

        let mut ports = PortLayout {
            inputs: port_channels(plugin.as_ptr(), extensions.audio.ports, true),
            outputs: port_channels(plugin.as_ptr(), extensions.audio.ports, false),
        };

        plugin_info.audio_inputs = ports.input_channel_total().max(2);
        plugin_info.audio_outputs = ports.output_channel_total().max(2);

        if ports.inputs.is_empty() {
            ports.inputs.push(2);
        }
        if ports.outputs.is_empty() {
            ports.outputs.push(2);
        }

        let audio = AudioConfig {
            sample_rate,
            max_frames,
            supports_f64: check_f64_support(plugin.as_ptr(), extensions.audio.ports),
        };

        Ok(Self {
            plugin,
            _entry_guard: entry_guard,
            _library: library,
            _host: host,
            host_state,
            extensions,
            info: plugin_info,
            audio,
            ports,
            flags: LifecycleFlags::default(),
        })
    }

    /// Load a CLAP plugin for editor/parameter/state work only — never for
    /// audio. The returned instance must NOT be `activate()`d, `process()`d, or
    /// `start_processing()`d; doing so is a misuse of an editor-only load.
    ///
    /// CLAP's `gui`, `params`, and `state` extensions work without activation,
    /// so this skips the audio-config negotiation a processing load needs. The
    /// sample rate / max-frames passed to the plugin are placeholders that are
    /// never used (no `activate()` call consumes them). Use this in the
    /// in-process GUI host, where audio runs in a separate instance/process.
    pub fn load_editor_only(bundle_path: &Path, library_path: Option<&Path>) -> Result<Self> {
        // Placeholder audio config: never used because the caller must not
        // activate this instance. A processing load uses `load_with_library`
        // with the real sample rate / block size instead.
        const EDITOR_ONLY_SAMPLE_RATE: f64 = 44_100.0;
        const EDITOR_ONLY_MAX_FRAMES: u32 = 512;
        Self::load_with_library(
            bundle_path,
            library_path,
            EDITOR_ONLY_SAMPLE_RATE,
            EDITOR_ONLY_MAX_FRAMES,
        )
    }
}

fn port_channels(
    plugin: *const clap_plugin,
    audio_ports: *const clap_plugin_audio_ports,
    is_input: bool,
) -> Vec<u32> {
    let Some(ext) = (unsafe { ext::opt(audio_ports) }) else {
        return Vec::new();
    };
    let (count_fn, get_fn) = match (ext.count, ext.get) {
        (Some(c), Some(g)) => (c, g),
        _ => return Vec::new(),
    };
    let count = unsafe { count_fn(plugin, is_input) };
    (0..count)
        .filter_map(|i| {
            let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
            unsafe { get_fn(plugin, i, is_input, &mut info) }.then_some(info.channel_count)
        })
        .collect()
}

fn check_f64_support(
    plugin: *const clap_plugin,
    audio_ports: *const clap_plugin_audio_ports,
) -> bool {
    let Some(ext) = (unsafe { ext::opt(audio_ports) }) else {
        return false;
    };
    let (count_fn, get_fn) = match (ext.count, ext.get) {
        (Some(c), Some(g)) => (c, g),
        _ => return false,
    };
    let count = unsafe { count_fn(plugin, false) };
    (0..count).any(|i| {
        let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
        let ok = unsafe { get_fn(plugin, i, false, &mut info) };
        ok && (info.flags & CLAP_AUDIO_PORT_SUPPORTS_64BITS) != 0
    })
}
