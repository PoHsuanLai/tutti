//! CLAP plugin instance.

mod audio;
mod config;
mod descriptor;
mod ext;
mod extensions;
mod handle;
mod params;
mod polling;
mod ports;
mod resources;
mod state;
mod undo;

pub use audio::{ClapSample, ProcessContext, ProcessOutput, ProcessOutputRef};
pub use params::ParamMapping;

use crate::error::{ClapError, LoadStage, Result};
use crate::events::{InputEventList, OutputEventList};
use crate::host::{ClapHost, HostState};
use crate::types::{MidiEvent, NoteExpressionValue, ParameterChanges, PluginInfo};
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_SUPPORTS_64BITS,
};
use clap_sys::plugin::clap_plugin;
use config::{AudioConfig, LifecycleFlags, PortLayout, ProcessScratch};
use descriptor::load_descriptor;
use extensions::ExtensionCache;
use handle::PluginHandle;
use smallvec::SmallVec;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Heap reservation for pooled per-call event scratch on a `ClapInstance`.
///
/// Sized once at activation so steady-state `process` calls avoid the
/// allocator.
const EVENT_SCRATCH_CAPACITY: usize = 256;
/// Heap reservation for the parameter-change queue list (one entry per
/// distinct param_id emitted by the plugin per block).
const PARAM_QUEUE_CAPACITY: usize = 16;

/// Global registry for CLAP entry init/deinit lifecycle.
///
/// The CLAP spec requires `clap_entry.init()` to be called once when a library
/// is first loaded and `clap_entry.deinit()` once when it's finally unloaded.
/// Many plugins do not tolerate repeated init/deinit cycles within the same
/// process — their global state becomes corrupted. This registry ensures
/// `init()` is called exactly once per library path for the lifetime of the
/// process. `deinit()` is intentionally NOT called, matching real-world DAW
/// behavior where plugins run in subprocesses that exit cleanly.
static ENTRY_REGISTRY: Mutex<Option<HashMap<PathBuf, bool>>> = Mutex::new(None);

/// RAII guard for CLAP entry lifetime. Does not call deinit on drop — the
/// entry stays initialized for the lifetime of the process.
pub(crate) struct EntryGuard {
    _path: PathBuf,
}

/// Register a CLAP entry for the given path.
/// Calls `init_fn` only on the first load of a given library. Subsequent
/// loads of the same library skip init (the entry is already initialized).
pub(crate) fn entry_registry_acquire(
    path: &Path,
    init_fn: unsafe extern "C" fn(*const i8) -> bool,
    path_cstr: &std::ffi::CString,
) -> std::result::Result<EntryGuard, String> {
    let mut registry = ENTRY_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let map = registry.get_or_insert_with(HashMap::new);

    if !map.contains_key(path) {
        if !unsafe { init_fn(path_cstr.as_ptr()) } {
            return Err("Entry init failed".to_string());
        }
        map.insert(path.to_path_buf(), true);
    }

    Ok(EntryGuard {
        _path: path.to_path_buf(),
    })
}

pub struct ClapInstance {
    // IMPORTANT: Drop order matters! Fields drop top-to-bottom.
    // `plugin` drops first so the plugin can still access the host while
    // destroy() runs; `_entry_guard` drops before `_library` so deinit()
    // runs while the library is still mapped.
    pub(crate) plugin: PluginHandle,
    _entry_guard: EntryGuard,
    _library: libloading::Library,
    _host: Box<ClapHost>,
    pub(crate) host_state: Arc<HostState>,
    pub(crate) extensions: ExtensionCache,
    pub(crate) info: PluginInfo,
    pub(crate) audio: AudioConfig,
    pub(crate) ports: PortLayout,
    pub(crate) flags: LifecycleFlags,
    /// Pre-allocated RT scratch, sized once in `activate()`.
    pub(crate) scratch_f32: ProcessScratch<f32>,
    pub(crate) scratch_f64: ProcessScratch<f64>,
    /// Pooled event lists, owned by the instance. `InputEventList`
    /// is refilled per call (UMP → CLAP conversion); `OutputEventList`
    /// is filled by the plugin's `try_push` callback. Both are cleared
    /// in place each block — heap capacity persists from `activate()`.
    pub(crate) input_events: InputEventList,
    pub(crate) output_events: OutputEventList,
    /// Pooled return-value buffers. `process` writes converted events
    /// into these so the call can return borrowed slices.
    pub(crate) out_midi: SmallVec<[MidiEvent; 64]>,
    pub(crate) out_param_changes: ParameterChanges,
    pub(crate) out_note_expressions: SmallVec<[NoteExpressionValue; 16]>,
}

// Safety: CLAP plugins are designed to be called from a single thread
unsafe impl Send for ClapInstance {}

impl ClapInstance {
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

        let plugin_init_fn = unsafe { &*plugin_ptr }
            .init
            .ok_or_else(|| ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Initialization,
                reason: "No plugin init function".to_string(),
            })?;

        if !unsafe { plugin_init_fn(plugin_ptr) } {
            return Err(ClapError::LoadFailed {
                path: bundle_path.to_path_buf(),
                stage: LoadStage::Initialization,
                reason: "Plugin init failed".to_string(),
            });
        }

        let plugin = PluginHandle::new(plugin_ptr);
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
            scratch_f32: ProcessScratch::new(),
            scratch_f64: ProcessScratch::new(),
            input_events: InputEventList::new(),
            output_events: OutputEventList::new(),
            out_midi: SmallVec::new(),
            out_param_changes: ParameterChanges::new(),
            out_note_expressions: SmallVec::new(),
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

    pub fn supports_f64(&self) -> bool {
        self.audio.supports_f64
    }

    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    pub fn sample_rate(&self) -> f64 {
        self.audio.sample_rate
    }

    pub fn block_size(&self) -> u32 {
        self.audio.max_frames
    }

    /// Debug-only guard: panic if called off the thread that created this
    /// instance's `HostState` (the main/UI thread). CLAP requires all calls
    /// except the audio-thread process path to run on the main thread; this
    /// turns a silent spec violation into a located panic in dev/test. No-op
    /// in release.
    #[inline]
    pub(crate) fn assert_main_thread(&self) {
        debug_assert_eq!(
            std::thread::current().id(),
            self.host_state.main_thread_id,
            "CLAP main-thread call made off the main thread"
        );
    }

    pub fn is_active(&self) -> bool {
        self.flags.active
    }

    pub fn is_processing(&self) -> bool {
        self.flags.processing
    }

    pub fn activate(&mut self) -> Result<()> {
        self.assert_main_thread();
        if self.flags.active {
            return Ok(());
        }

        let plugin_ref = unsafe { self.plugin.as_ref() };
        let activate_fn = plugin_ref.activate.ok_or(ClapError::NotActivated)?;

        if !unsafe {
            activate_fn(
                self.plugin.as_ptr(),
                self.audio.sample_rate,
                1,
                self.audio.max_frames,
            )
        } {
            return Err(ClapError::LoadFailed {
                path: PathBuf::new(),
                stage: LoadStage::Activation,
                reason: "Activate failed".to_string(),
            });
        }

        // Pre-allocate RT scratch now that the port layout is known. We
        // size both f32 and f64 scratches so the RT path is allocation-free
        // regardless of sample type the caller uses.
        let input_total = self.ports.input_channel_total();
        let output_total = self.ports.output_channel_total();
        let max_frames = self.audio.max_frames as usize;
        let num_in = self.ports.inputs.len();
        let num_out = self.ports.outputs.len();
        self.scratch_f32
            .resize_for(input_total, output_total, max_frames, num_in, num_out);
        if self.audio.supports_f64 {
            self.scratch_f64
                .resize_for(input_total, output_total, max_frames, num_in, num_out);
        }

        // Pre-allocate event scratch so steady-state `process` calls never
        // touch the allocator. EVENT_SCRATCH_CAPACITY covers typical
        // per-block event counts (≤256). PARAM_QUEUE_CAPACITY covers
        // distinct param_ids per block.
        self.input_events.reserve(EVENT_SCRATCH_CAPACITY);
        self.output_events.reserve(EVENT_SCRATCH_CAPACITY);
        self.out_midi.reserve(EVENT_SCRATCH_CAPACITY);
        self.out_param_changes.queues.reserve(PARAM_QUEUE_CAPACITY);
        self.out_note_expressions.reserve(EVENT_SCRATCH_CAPACITY);

        self.flags.active = true;
        Ok(())
    }

    pub fn deactivate(&mut self) {
        if !self.flags.active {
            return;
        }
        if self.flags.processing {
            self.stop_processing();
        }
        let plugin_ref = unsafe { self.plugin.as_ref() };
        if let Some(deactivate_fn) = plugin_ref.deactivate {
            unsafe { deactivate_fn(self.plugin.as_ptr()) };
        }
        self.flags.active = false;
    }

    pub fn start_processing(&mut self) -> Result<()> {
        if !self.flags.active {
            self.activate()?;
        }
        if self.flags.processing {
            return Ok(());
        }

        // Publish the current thread as the audio thread before the plugin's
        // start_processing runs — plugins commonly call is_audio_thread from
        // inside it. This is the only place we pay the Arc allocation; the
        // per-buffer do_process path only reads the ArcSwapOption.
        self.host_state
            .audio_thread_id
            .store(Some(std::sync::Arc::new(std::thread::current().id())));

        let plugin_ref = unsafe { self.plugin.as_ref() };
        if let Some(start_fn) = plugin_ref.start_processing {
            if !unsafe { start_fn(self.plugin.as_ptr()) } {
                return Err(ClapError::ProcessError(
                    "Start processing failed".to_string(),
                ));
            }
        }

        self.flags.processing = true;
        Ok(())
    }

    pub fn stop_processing(&mut self) {
        if !self.flags.processing {
            return;
        }
        let plugin_ref = unsafe { self.plugin.as_ref() };
        if let Some(stop_fn) = plugin_ref.stop_processing {
            unsafe { stop_fn(self.plugin.as_ptr()) };
        }
        self.host_state.audio_thread_id.store(None);
        self.flags.processing = false;
    }

    pub fn set_sample_rate(&mut self, sample_rate: f64) -> &mut Self {
        if (self.audio.sample_rate - sample_rate).abs() < f64::EPSILON {
            return self;
        }
        if self.flags.active {
            self.deactivate();
        }
        self.audio.sample_rate = sample_rate;
        self
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

impl Drop for ClapInstance {
    fn drop(&mut self) {
        // Destroy GUI before tearing down the plugin — CLAP spec requires
        // gui.destroy() before deactivate()/plugin.destroy().
        self.close_editor();

        if self.flags.processing {
            let plugin_ref = unsafe { self.plugin.as_ref() };
            if let Some(stop_fn) = plugin_ref.stop_processing {
                unsafe { stop_fn(self.plugin.as_ptr()) };
            }
        }

        if self.flags.active {
            let plugin_ref = unsafe { self.plugin.as_ref() };
            if let Some(deactivate_fn) = plugin_ref.deactivate {
                unsafe { deactivate_fn(self.plugin.as_ptr()) };
            }
        }

        // PluginHandle::Drop calls destroy(); EntryGuard::Drop is a no-op;
        // library unloads last.
    }
}

#[cfg(test)]
mod tests {
    use super::polling::{context_menu_builder_add_item, context_menu_builder_supports};
    use crate::types::ContextMenuItem;
    use clap_sys::ext::context_menu::{
        clap_context_menu_builder, clap_context_menu_entry, CLAP_CONTEXT_MENU_ITEM_ENTRY,
        CLAP_CONTEXT_MENU_ITEM_SEPARATOR,
    };
    use std::ffi::c_void;

    #[test]
    fn test_context_menu_builder_null_builder() {
        unsafe {
            let result = context_menu_builder_add_item(
                std::ptr::null(),
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR,
                std::ptr::null(),
            );
            assert!(!result);
        }
    }

    #[test]
    fn test_context_menu_builder_null_ctx() {
        let builder = clap_context_menu_builder {
            ctx: std::ptr::null_mut(),
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        unsafe {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR,
                std::ptr::null(),
            );
            assert!(!result);
        }
    }

    #[test]
    fn test_context_menu_builder_separator() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        unsafe {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR,
                std::ptr::null(),
            );
            assert!(result);
        }
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], ContextMenuItem::Separator));
    }

    #[test]
    fn test_context_menu_builder_entry_null_data() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        unsafe {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_ENTRY,
                std::ptr::null(),
            );
            assert!(!result);
        }
        assert!(items.is_empty());
    }

    #[test]
    fn test_context_menu_builder_entry_with_data() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };

        let label = std::ffi::CString::new("Test Entry").unwrap();
        let entry = clap_context_menu_entry {
            label: label.as_ptr(),
            is_enabled: true,
            action_id: 42,
        };

        unsafe {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_ENTRY,
                &entry as *const clap_context_menu_entry as *const c_void,
            );
            assert!(result);
        }
        assert_eq!(items.len(), 1);
        match &items[0] {
            ContextMenuItem::Entry {
                label,
                is_enabled,
                action_id,
            } => {
                assert_eq!(label, "Test Entry");
                assert!(*is_enabled);
                assert_eq!(*action_id, 42);
            }
            _ => panic!("Expected Entry"),
        }
    }

    #[test]
    fn test_context_menu_builder_unknown_type() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        unsafe {
            let result = context_menu_builder_add_item(&builder, 9999, std::ptr::null());
            assert!(!result);
        }
        assert!(items.is_empty());
    }

    #[test]
    fn test_context_menu_builder_supports() {
        unsafe {
            assert!(context_menu_builder_supports(
                std::ptr::null(),
                CLAP_CONTEXT_MENU_ITEM_ENTRY
            ));
            assert!(context_menu_builder_supports(
                std::ptr::null(),
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR
            ));
            assert!(!context_menu_builder_supports(std::ptr::null(), 9999));
        }
    }
}
