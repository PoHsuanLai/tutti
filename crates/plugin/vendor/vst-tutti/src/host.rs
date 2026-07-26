//! Host specific structures.

use num_enum::{IntoPrimitive, TryFromPrimitive};
use num_traits::Float;

use libloading::Library;
use std::cell::UnsafeCell;
use std::convert::TryFrom;
use std::error::Error;
use std::ffi::CString;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::os::raw::c_void;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::Arc;
use std::{fmt, ptr, slice};

use crate::{
    api::{self, consts::*, AEffect, PluginFlags, PluginMain, Supported, TimeInfo},
    buffer::AudioBuffer,
    channels::ChannelInfo,
    editor::{Editor, Rect},
    interfaces,
    plugin::{self, Category, HostCallback, Info, Plugin, PluginParameters},
};

#[repr(i32)]
#[derive(Clone, Copy, Debug, TryFromPrimitive, IntoPrimitive)]
#[doc(hidden)]
pub enum OpCode {
    /// [index]: parameter index
    /// [opt]: parameter value
    Automate = 0,
    /// [return]: host vst version (e.g. 2400 for VST 2.4)
    Version,
    /// [return]: current plugin ID (useful for shell plugins to figure out which plugin to load in
    ///           `VSTPluginMain()`).
    CurrentId,
    /// No arguments. Give idle time to Host application, e.g. if plug-in editor is doing mouse
    /// tracking in a modal loop.
    Idle,
    /// Deprecated.
    _PinConnected = 4,

    /// Deprecated.
    _WantMidi = 6, // Not a typo
    /// [value]: request mask. see `VstTimeInfoFlags`
    /// [return]: `VstTimeInfo` pointer or null if not supported.
    GetTime,
    /// Inform host that the plugin has MIDI events ready to be processed. Should be called at the
    /// end of `Plugin::process`.
    /// [ptr]: `VstEvents*` the events to be processed.
    /// [return]: 1 if supported and processed OK.
    ProcessEvents,
    /// Deprecated.
    _SetTime,
    /// Deprecated.
    _TempoAt,
    /// Deprecated.
    _GetNumAutomatableParameters,
    /// Deprecated.
    _GetParameterQuantization,

    /// Notifies the host that the input/output setup has changed. This can allow the host to check
    /// numInputs/numOutputs or call `getSpeakerArrangement()`.
    /// [return]: 1 if supported.
    IOChanged,

    /// Deprecated.
    _NeedIdle,

    /// Request the host to resize the plugin window.
    /// [index]: new width.
    /// [value]: new height.
    SizeWindow,
    /// [return]: the current sample rate.
    GetSampleRate,
    /// [return]: the current block size.
    GetBlockSize,
    /// [return]: the input latency in samples.
    GetInputLatency,
    /// [return]: the output latency in samples.
    GetOutputLatency,

    /// Deprecated.
    _GetPreviousPlug,
    /// Deprecated.
    _GetNextPlug,
    /// Deprecated.
    _WillReplaceOrAccumulate,

    /// [return]: the current process level, see `VstProcessLevels`
    GetCurrentProcessLevel,
    /// [return]: the current automation state, see `VstAutomationStates`
    GetAutomationState,

    /// The plugin is ready to begin offline processing.
    /// [index]: number of new audio files.
    /// [value]: number of audio files.
    /// [ptr]: `AudioFile*` the host audio files. Flags can be updated from plugin.
    OfflineStart,
    /// Called by the plugin to read data.
    /// [index]: (bool)
    ///    VST offline processing allows a plugin to overwrite existing files. If this value is
    ///    true then the host will read the original file's samples, but if it is false it will
    ///    read the samples which the plugin has written via `OfflineWrite`
    /// [value]: see `OfflineOption`
    /// [ptr]: `OfflineTask*` describing the task.
    /// [return]: 1 on success
    OfflineRead,
    /// Called by the plugin to write data.
    /// [value]: see `OfflineOption`
    /// [ptr]: `OfflineTask*` describing the task.
    OfflineWrite,
    /// Unknown. Used in offline processing.
    OfflineGetCurrentPass,
    /// Unknown. Used in offline processing.
    OfflineGetCurrentMetaPass,

    /// Deprecated.
    _SetOutputSampleRate,
    /// Deprecated.
    _GetOutputSpeakerArrangement,

    /// Get the vendor string.
    /// [ptr]: `char*` for vendor string, limited to `MAX_VENDOR_STR_LEN`.
    GetVendorString,
    /// Get the product string.
    /// [ptr]: `char*` for vendor string, limited to `MAX_PRODUCT_STR_LEN`.
    GetProductString,
    /// [return]: vendor-specific version
    GetVendorVersion,
    /// Vendor specific handling.
    VendorSpecific,

    /// Deprecated.
    _SetIcon,

    /// Check if the host supports a feature.
    /// [ptr]: `char*` can do string
    /// [return]: 1 if supported
    CanDo,
    /// Get the language of the host.
    /// [return]: `VstHostLanguage`
    GetLanguage,

    /// Deprecated.
    _OpenWindow,
    /// Deprecated.
    _CloseWindow,

    /// Get the current directory.
    /// [return]: `FSSpec` on OS X, `char*` otherwise
    GetDirectory,
    /// Tell the host that the plugin's parameters have changed, refresh the UI.
    ///
    /// No arguments.
    UpdateDisplay,
    /// Tell the host that if needed, it should record automation data for a control.
    ///
    /// Typically called when the plugin editor begins changing a control.
    ///
    /// [index]: index of the control.
    /// [return]: true on success.
    BeginEdit,
    /// A control is no longer being changed.
    ///
    /// Typically called after the plugin editor is done.
    ///
    /// [index]: index of the control.
    /// [return]: true on success.
    EndEdit,
    /// Open the host file selector.
    /// [ptr]: `VstFileSelect*`
    /// [return]: true on success.
    OpenFileSelector,
    /// Close the host file selector.
    /// [ptr]: `VstFileSelect*`
    /// [return]: true on success.
    CloseFileSelector,

    /// Deprecated.
    _EditFile,
    /// Deprecated.
    /// [ptr]: char[2048] or sizeof (FSSpec).
    /// [return]: 1 if supported.
    _GetChunkFile,
    /// Deprecated.
    _GetInputSpeakerArrangement,
}

/// Implemented by all VST hosts.
#[allow(unused_variables)]
pub trait Host {
    /// Automate a parameter; the value has been changed.
    fn automate(&self, index: i32, value: f32) {}

    /// Signal that automation of a parameter started (the knob has been touched / mouse button down).
    fn begin_edit(&self, index: i32) {}

    /// Signal that automation of a parameter ended (the knob is no longer been touched / mouse button up).
    fn end_edit(&self, index: i32) {}

    /// Get the plugin ID of the currently loading plugin.
    ///
    /// This is only useful for shell plugins where this value will change the plugin returned.
    /// `TODO: implement shell plugins`
    fn get_plugin_id(&self) -> i32 {
        // TODO: Handle this properly
        0
    }

    /// An idle call.
    ///
    /// This is useful when the plugin is doing something such as mouse tracking in the UI.
    fn idle(&self) {}

    /// Get vendor and product information.
    ///
    /// Returns a tuple in the form of `(version, vendor_name, product_name)`.
    fn get_info(&self) -> (isize, String, String) {
        (1, "vendor string".to_owned(), "product string".to_owned())
    }

    /// Handle incoming events from the plugin.
    fn process_events(&self, events: &api::Events) {}

    /// Get time information.
    fn get_time_info(&self, mask: i32) -> Option<TimeInfo> {
        None
    }

    /// Get block size.
    fn get_block_size(&self) -> isize {
        0
    }

    /// Get the current sample rate (`audioMasterGetSampleRate`).
    fn get_sample_rate(&self) -> f32 {
        44_100.0
    }

    /// Honor a plugin-requested editor-window resize (`audioMasterSizeWindow`).
    /// `index` = new width, `value` = new height. Return `true` if the host
    /// resized the window.
    fn size_window(&self, _index: i32, _value: isize) -> bool {
        false
    }

    /// The plugin's input/output setup changed (`audioMasterIOChanged`).
    /// Return `true` if handled.
    fn io_changed(&self) -> bool {
        false
    }

    /// Report the host's input latency in samples (`audioMasterGetInputLatency`).
    fn get_input_latency(&self) -> isize {
        0
    }

    /// Report the host's output latency in samples (`audioMasterGetOutputLatency`).
    fn get_output_latency(&self) -> isize {
        0
    }

    /// Report the current process level (`audioMasterGetCurrentProcessLevel`,
    /// `VstProcessLevels`): 0 = unknown, 2 = realtime, 4 = offline.
    fn get_process_level(&self) -> isize {
        0
    }

    /// Report the host's automation state (`audioMasterGetAutomationState`,
    /// `VstAutomationStates`): 0 = unsupported / not applicable.
    fn get_automation_state(&self) -> isize {
        0
    }

    /// Refresh UI after the plugin's parameters changed.
    ///
    /// Note: some hosts will call some `PluginParameters` methods from within the `update_display`
    /// call, including `get_parameter`, `get_parameter_label`, `get_parameter_name`
    /// and `get_parameter_text`.
    fn update_display(&self) {}
}

/// All possible errors that can occur when loading a VST plugin.
#[derive(Debug)]
pub enum PluginLoadError {
    /// Could not load given path.
    InvalidPath,

    /// Given path is not a VST plugin.
    NotAPlugin,

    /// Failed to create an instance of this plugin.
    ///
    /// This can happen for many reasons, such as if the plugin requires a different version of
    /// the VST API to be used, or due to improper licensing.
    InstanceFailed,

    /// The API version which the plugin used is not supported by this library.
    InvalidApiVersion,
}

impl fmt::Display for PluginLoadError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::PluginLoadError::*;
        let description = match self {
            InvalidPath => "Could not open the requested path",
            NotAPlugin => "The given path does not contain a VST2.4 compatible library",
            InstanceFailed => "Failed to create a plugin instance",
            InvalidApiVersion => "The plugin API version is not compatible with this library",
        };
        write!(f, "{}", description)
    }
}

impl Error for PluginLoadError {}

/// Wrapper for an externally loaded VST plugin.
///
/// The only functionality this struct provides is loading plugins, which can be done via the
/// [`load`](#method.load) method.
pub struct PluginLoader<T: Host> {
    main: PluginMain,
    lib: Arc<Library>,
    host: Arc<T>,
}

/// An instance of an externally loaded VST plugin.
#[allow(dead_code)] // To keep `lib` around.
pub struct PluginInstance {
    params: Arc<PluginParametersInstance>,
    /// The `dlopen` handle, deliberately leaked on drop.
    ///
    /// JUCE-based plugins (and several others) crash inside their static
    /// destructor sequence when the module is unloaded, so hosts do not
    /// `dlclose` them. `ManuallyDrop` is how that is expressed here — note it
    /// covers *only* the unload. `effClose` still runs (see [`Drop`]); the two
    /// are separate events and conflating them is what leaked live plugin
    /// instances and licence seats.
    lib: ManuallyDrop<Arc<Library>>,
    info: Info,
    is_editor_active: bool,
    /// `effFlagsCanReplacing` as reported in `AEffect::flags`.
    ///
    /// Mandatory in VST2.4 and set by essentially every plugin, but a plugin
    /// that clears it is telling the host that `processReplacing` is not
    /// installed. Captured here rather than added to [`Info`] because `Info` is
    /// the *plugin-authoring* struct: it is what a plugin returns from
    /// `get_info()` to have the flags built for it, so a `can_replacing` field
    /// there would be a host-only concern in a plugin-side type.
    can_replacing: bool,
    /// `effFlagsCanDoubleReplacing`, mirroring [`Self::can_replacing`] for the
    /// `f64` path. (Also surfaced to plugin authors as `Info::f64_precision`.)
    can_double_replacing: bool,
}

struct PluginParametersInstance {
    effect: UnsafeCell<*mut AEffect>,
}

unsafe impl Send for PluginParametersInstance {}
unsafe impl Sync for PluginParametersInstance {}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        // `effClose`. The plugin frees its own `AEffect` in response, so this
        // must happen exactly once — and it *must* happen: a licensed plugin
        // releases its session seat here, and skipping it leaks one live
        // instance (and one licence) per A/B of a slot.
        self.dispatch(plugin::OpCode::Shutdown, 0, 0, ptr::null_mut(), 0.0);

        // `self.lib` is `ManuallyDrop`, so the `Arc<Library>` is never released
        // and the loader never `dlclose`s the module. See the field's docs.
    }
}

/// The editor of an externally loaded VST plugin.
struct EditorInstance {
    params: Arc<PluginParametersInstance>,
    is_open: bool,
}

impl EditorInstance {
    fn get_rect(&self) -> Option<Rect> {
        let mut rect: *mut Rect = std::ptr::null_mut();
        let rect_ptr: *mut *mut Rect = &mut rect;

        let result = self.params.dispatch(
            plugin::OpCode::EditorGetRect,
            0,
            0,
            rect_ptr as *mut c_void,
            0.0,
        );

        if result == 0 || rect.is_null() {
            return None;
        }
        Some(unsafe { *rect }) // TODO: Who owns rect? Who should free the memory?
    }
}

impl Editor for EditorInstance {
    fn size(&self) -> (i32, i32) {
        // Assuming coordinate origins from top-left
        match self.get_rect() {
            None => (0, 0),
            Some(rect) => (
                (rect.right - rect.left) as i32,
                (rect.bottom - rect.top) as i32,
            ),
        }
    }

    fn position(&self) -> (i32, i32) {
        // Assuming coordinate origins from top-left
        match self.get_rect() {
            None => (0, 0),
            Some(rect) => (rect.left as i32, rect.top as i32),
        }
    }

    fn close(&mut self) {
        self.params
            .dispatch(plugin::OpCode::EditorClose, 0, 0, ptr::null_mut(), 0.0);
        self.is_open = false;
    }

    fn open(&mut self, parent: *mut c_void) -> bool {
        let result = self
            .params
            .dispatch(plugin::OpCode::EditorOpen, 0, 0, parent, 0.0);

        let opened = result == 1;
        if opened {
            self.is_open = true;
        }

        opened
    }

    fn is_open(&mut self) -> bool {
        self.is_open
    }
}

impl<T: Host> PluginLoader<T> {
    /// Load a plugin at the given path with the given host.
    ///
    /// The host is passed as a plain `Arc<T>`, **not** `Arc<Mutex<T>>`.
    /// `callback_wrapper` — the C function pointer the plugin calls — is
    /// invoked from the plugin's *audio* thread for `audioMasterGetTime`
    /// (from inside `processReplacing` in nearly every synth),
    /// `audioMasterAutomate` and `audioMasterProcessEvents`, and from the
    /// *UI* thread for `audioMasterSizeWindow` / `audioMasterUpdateDisplay`.
    /// A `std::sync::Mutex` shared by both is a priority inversion: the audio
    /// thread blocks on the GUI thread and the stream drops out. `Host` takes
    /// `&self` throughout, so the lock bought nothing but the inversion (and a
    /// `PoisonError` unwind across the FFI boundary). Implementors are
    /// responsible for their own interior mutability, which is what a real-time
    /// host wants anyway: `ArcSwap` / atomics / lock-free channels.
    ///
    /// Upon success, this method returns a [`PluginLoader`](.) object which you can use to call
    /// [`instance`](#method.instance) to create a new instance of the plugin.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use std::sync::Arc;
    /// # use vst_tutti::host::{Host, PluginLoader};
    /// # let path = Path::new(".");
    /// # struct MyHost;
    /// # impl MyHost { fn new() -> MyHost { MyHost } }
    /// # impl Host for MyHost {
    /// #     fn automate(&self, _: i32, _: f32) {}
    /// #     fn get_plugin_id(&self) -> i32 { 0 }
    /// # }
    /// // ...
    /// let host = Arc::new(MyHost::new());
    ///
    /// let mut plugin = PluginLoader::load(path, host.clone()).unwrap();
    ///
    /// let instance = plugin.instance().unwrap();
    /// // ...
    /// ```
    ///
    /// # Linux/Windows
    ///   * This should be a path to the library, typically ending in `.so`/`.dll`.
    ///   * Possible full path: `/home/overdrivenpotato/.vst/u-he/Zebra2.64.so`
    ///   * Possible full path: `C:\Program Files (x86)\VSTPlugins\iZotope Ozone 5.dll`
    ///
    /// # OS X
    ///   * This should point to the mach-o file within the `.vst` bundle.
    ///   * Plugin: `/Library/Audio/Plug-Ins/VST/iZotope Ozone 5.vst`
    ///   * Possible full path:
    ///     `/Library/Audio/Plug-Ins/VST/iZotope Ozone 5.vst/Contents/MacOS/PluginHooksVST`
    pub fn load(path: &Path, host: Arc<T>) -> Result<PluginLoader<T>, PluginLoadError> {
        // Try loading the library at the given path
        unsafe {
            let lib = match Library::new(path) {
                Ok(l) => l,
                Err(_) => return Err(PluginLoadError::InvalidPath),
            };

            Ok(PluginLoader {
                main:
                    // Search the library for the VSTAPI entry point
                    match lib.get(b"VSTPluginMain") {
                        Ok(s) => *s,
                        _ => return Err(PluginLoadError::NotAPlugin),
                    }
                ,
                lib: Arc::new(lib),
                host,
            })
        }
    }

    /// Call the VST entry point and retrieve a (possibly null) pointer.
    unsafe fn call_main(&mut self) -> *mut AEffect {
        LOAD_POINTER = Box::into_raw(Box::new(Arc::clone(&self.host))) as *mut c_void;
        (self.main)(callback_wrapper::<T>)
    }

    /// Try to create an instance of this VST plugin.
    ///
    /// If the instance is successfully created, a [`PluginInstance`](struct.PluginInstance.html)
    /// is returned. This struct implements the [`Plugin` trait](../plugin/trait.Plugin.html).
    pub fn instance(&mut self) -> Result<PluginInstance, PluginLoadError> {
        // Call the plugin main function. This also passes the plugin main function as the closure
        // could not return an error if the symbol wasn't found
        let effect = unsafe { self.call_main() };

        if effect.is_null() {
            return Err(PluginLoadError::InstanceFailed);
        }

        unsafe {
            // Move the host to the heap and add it to the `AEffect` struct for future reference
            (*effect).reserved1 = Box::into_raw(Box::new(Arc::clone(&self.host))) as isize;
        }

        let instance = PluginInstance::new(effect, Arc::clone(&self.lib));

        let api_ver = instance.dispatch(plugin::OpCode::GetApiVersion, 0, 0, ptr::null_mut(), 0.0);
        if api_ver >= 2400 {
            Ok(instance)
        } else {
            trace!("Could not load plugin with api version {}", api_ver);
            Err(PluginLoadError::InvalidApiVersion)
        }
    }
}

impl PluginInstance {
    fn new(effect: *mut AEffect, lib: Arc<Library>) -> PluginInstance {
        use plugin::OpCode as op;

        let params = Arc::new(PluginParametersInstance {
            effect: UnsafeCell::new(effect),
        });
        let mut plug = PluginInstance {
            params,
            lib: ManuallyDrop::new(lib),
            info: Default::default(),
            is_editor_active: false,
            can_replacing: false,
            can_double_replacing: false,
        };

        unsafe {
            let effect: &AEffect = &*effect;
            let flags = PluginFlags::from_bits_truncate(effect.flags);

            plug.can_replacing = flags.intersects(PluginFlags::CAN_REPLACING);
            plug.can_double_replacing = flags.intersects(PluginFlags::CAN_DOUBLE_REPLACING);

            plug.info = Info {
                name: plug.read_string(op::GetProductName, MAX_PRODUCT_STR_LEN),
                vendor: plug.read_string(op::GetVendorName, MAX_VENDOR_STR_LEN),

                presets: effect.numPrograms,
                parameters: effect.numParams,
                inputs: effect.numInputs,
                outputs: effect.numOutputs,

                midi_inputs: 0,
                midi_outputs: 0,

                unique_id: effect.uniqueId,
                version: effect.version,

                category: Category::try_from(plug.opcode(op::GetCategory))
                    .unwrap_or(Category::Unknown),

                initial_delay: effect.initialDelay,

                preset_chunks: flags.intersects(PluginFlags::PROGRAM_CHUNKS),
                f64_precision: flags.intersects(PluginFlags::CAN_DOUBLE_REPLACING),
                silent_when_stopped: flags.intersects(PluginFlags::NO_SOUND_IN_STOP),
            };
        }

        plug
    }

    /// Ask the plugin to fill an `api::ChannelProperties` and return what it
    /// wrote, or an all-zero struct if it wrote nothing.
    ///
    /// `effGetInputProperties` / `effGetOutputProperties` are *optional*
    /// opcodes: an unimplemented one falls through the plugin's dispatcher and
    /// returns 0 without touching the buffer. Handing the plugin a
    /// `MaybeUninit` and then calling `assume_init()` unconditionally therefore
    /// read uninitialised memory — UB in its own right, before any garbage
    /// `flags` value ever reached [`ChannelInfo`]. We hand over a zeroed struct
    /// instead (all-zero is a valid `ChannelProperties`: no flags,
    /// `arrangement_type = 0` = `Mono`, empty labels), then sanitise the one
    /// field whose type constrains its bit pattern.
    fn channel_properties(&self, opcode: plugin::OpCode, index: i32) -> api::ChannelProperties {
        // SAFETY: `ChannelProperties` is a `#[repr(C)]` POD of byte arrays, an
        // `i32` and a `#[repr(i32)]` enum whose `0` discriminant is `Mono`, so
        // the all-zero bit pattern is a valid, in-range value for every field.
        let mut props: MaybeUninit<api::ChannelProperties> = MaybeUninit::zeroed();
        let ptr = props.as_mut_ptr() as *mut c_void;

        self.dispatch(opcode, index, 0, ptr, 0.0);

        // `arrangement_type` is a `#[repr(i32)]` enum the *plugin* wrote. Its
        // valid discriminants are -2..=27; anything else is not a value of the
        // type, and simply `match`ing on it (which `ChannelInfo::from` does)
        // would be UB. Read the raw i32 out of the still-`MaybeUninit` struct
        // and overwrite an out-of-range one with `Custom` before the value is
        // ever materialised as the enum.
        //
        // SAFETY: `props` was zeroed above, so every byte is initialised; the
        // field offset comes from `addr_of_mut!` on the same allocation.
        unsafe {
            let ty = std::ptr::addr_of_mut!((*props.as_mut_ptr()).arrangement_type);
            let raw = ty.cast::<i32>().read();
            const MIN: i32 = api::SpeakerArrangementType::Custom as i32; // -2
            const MAX: i32 = api::SpeakerArrangementType::Surround102 as i32;
            if !(MIN..=MAX).contains(&raw) {
                warn!("plugin reported out-of-range speaker arrangement {raw}; treating as Custom");
                ty.cast::<i32>().write(MIN);
            }
        }

        // Initialised either way: we zeroed it, and the plugin may have
        // overwritten it in place.
        unsafe { props.assume_init() }
    }
}

/// Silence every output channel of `buffer`.
///
/// Used both as the "there is nothing safe to call" result and as the required
/// pre-clear for the deprecated accumulating `process` entry point, which adds
/// into the outputs rather than overwriting them.
fn zero_outputs<T: Float>(buffer: &mut AudioBuffer<'_, T>) {
    let (_, mut outputs) = buffer.split();
    for i in 0..outputs.len() {
        for sample in outputs.get_mut(i).iter_mut() {
            *sample = T::zero();
        }
    }
}

trait Dispatch {
    fn get_effect(&self) -> *mut AEffect;

    /// Send a dispatch message to the plugin.
    fn dispatch(
        &self,
        opcode: plugin::OpCode,
        index: i32,
        value: isize,
        ptr: *mut c_void,
        opt: f32,
    ) -> isize {
        let dispatcher = unsafe { (*self.get_effect()).dispatcher };
        if (dispatcher as *mut u8).is_null() {
            panic!("Plugin was not loaded correctly.");
        }
        dispatcher(self.get_effect(), opcode.into(), index, value, ptr, opt)
    }

    /// Send a lone opcode with no parameters.
    fn opcode(&self, opcode: plugin::OpCode) -> isize {
        self.dispatch(opcode, 0, 0, ptr::null_mut(), 0.0)
    }

    /// Like `dispatch`, except takes a `&str` to send via `ptr`.
    fn write_string(
        &self,
        opcode: plugin::OpCode,
        index: i32,
        value: isize,
        string: &str,
        opt: f32,
    ) -> isize {
        let string = CString::new(string).expect("Invalid string data");
        self.dispatch(
            opcode,
            index,
            value,
            string.as_bytes().as_ptr() as *mut c_void,
            opt,
        )
    }

    fn read_string(&self, opcode: plugin::OpCode, max: usize) -> String {
        self.read_string_param(opcode, 0, 0, 0.0, max)
    }

    fn read_string_param(
        &self,
        opcode: plugin::OpCode,
        index: i32,
        value: isize,
        opt: f32,
        max: usize,
    ) -> String {
        let mut buf = vec![0; max];
        self.dispatch(opcode, index, value, buf.as_mut_ptr() as *mut c_void, opt);
        String::from_utf8_lossy(&buf)
            .chars()
            .take_while(|c| *c != '\0')
            .collect()
    }
}

impl Dispatch for PluginInstance {
    fn get_effect(&self) -> *mut AEffect {
        self.params.get_effect()
    }
}

impl Dispatch for PluginParametersInstance {
    fn get_effect(&self) -> *mut AEffect {
        unsafe { *self.effect.get() }
    }
}

impl PluginParametersInstance {
    /// Read an `effGetChunk` blob (`index == 1` for the current preset,
    /// `index == 0` for the whole bank), returning empty on any answer the
    /// plugin is allowed to give but that is not a readable buffer.
    ///
    /// Three plugin behaviours must not become UB on the project-save path:
    ///
    /// * `effFlagsProgramChunks` set but no preset loaded yet — the plugin
    ///   returns `0` and never writes the out-pointer, leaving it null.
    ///   `slice::from_raw_parts(null, 0)` is UB even at length zero, because
    ///   the pointer must always be non-null and aligned.
    /// * an error return of `-1`, which `as usize` turned into `usize::MAX`
    ///   and then a `Vec` allocation of 16 exbibytes (or a wild read).
    /// * a positive length with a null pointer, from a plugin that computed a
    ///   size but failed to hand back the buffer.
    fn get_chunk(&self, index: i32) -> Vec<u8> {
        // Create a pointer that can be updated from the plugin.
        let mut ptr: *mut u8 = ptr::null_mut();
        let len = self.dispatch(
            plugin::OpCode::GetData,
            index,
            0,
            &mut ptr as *mut *mut u8 as *mut c_void,
            0.0,
        );

        // SAFETY: `copy_chunk` rejects every pointer/length pair that is not a
        // readable buffer before dereferencing. Where it does read, the plugin
        // owns the buffer and the VST2.4 contract keeps it valid until the next
        // dispatch — longer than this copy.
        unsafe { copy_chunk(ptr, len) }
    }
}

/// Copy an `effGetChunk` result, rejecting the pointer/length pairs a plugin is
/// allowed to produce but that are not a readable buffer.
///
/// Split out from [`PluginParametersInstance::get_chunk`] so the validation is
/// testable without a loaded plugin.
///
/// # Safety
/// If `len > 0` and `ptr` is non-null, `ptr` must be valid for reads of `len`
/// bytes.
unsafe fn copy_chunk(ptr: *mut u8, len: isize) -> Vec<u8> {
    if len <= 0 {
        if len < 0 {
            warn!("plugin returned a negative effGetChunk length ({len}); treating as empty");
        }
        return Vec::new();
    }
    if ptr.is_null() {
        warn!("plugin reported {len} bytes of chunk data but left the pointer null");
        return Vec::new();
    }
    unsafe { slice::from_raw_parts(ptr, len as usize) }.to_vec()
}

impl Plugin for PluginInstance {
    fn get_info(&self) -> plugin::Info {
        self.info.clone()
    }

    fn new(_host: HostCallback) -> Self {
        // Plugin::new is only called on client side and PluginInstance is only used on host side
        unreachable!()
    }

    fn init(&mut self) {
        self.opcode(plugin::OpCode::Initialize);
    }

    fn set_sample_rate(&mut self, rate: f32) {
        self.dispatch(plugin::OpCode::SetSampleRate, 0, 0, ptr::null_mut(), rate);
    }

    fn set_block_size(&mut self, size: i64) {
        self.dispatch(
            plugin::OpCode::SetBlockSize,
            0,
            size as isize,
            ptr::null_mut(),
            0.0,
        );
    }

    fn resume(&mut self) {
        self.dispatch(plugin::OpCode::StateChanged, 0, 1, ptr::null_mut(), 0.0);
    }

    fn suspend(&mut self) {
        self.dispatch(plugin::OpCode::StateChanged, 0, 0, ptr::null_mut(), 0.0);
    }

    fn vendor_specific(&mut self, index: i32, value: isize, ptr: *mut c_void, opt: f32) -> isize {
        self.dispatch(plugin::OpCode::VendorSpecific, index, value, ptr, opt)
    }

    fn can_do(&self, can_do: plugin::CanDo) -> Supported {
        let s: String = can_do.into();
        // `Supported::from` is total: undocumented `effCanDo` returns (a string
        // length, an uninitialised stack slot) become `Supported::Custom`.
        // Panicking here would abort during *load*, since the host queries
        // canDo while probing every plugin.
        Supported::from(self.write_string(plugin::OpCode::CanDo, 0, 0, &s, 0.0))
    }

    fn get_tail_size(&self) -> isize {
        self.opcode(plugin::OpCode::GetTailSize)
    }

    /// Render one block.
    ///
    /// Prefers `processReplacing`, but only when the plugin both advertises
    /// `effFlagsCanReplacing` *and* actually installed the pointer. VST2.4
    /// makes the replacing path mandatory, so the check looks redundant — it is
    /// not: an unguarded call to a null `processReplacing` is a jump to address
    /// 0 in-process, which takes the whole DAW down, and a plugin can report
    /// api version >= 2400 while leaving the slot null. When replacing is
    /// unavailable we fall back to the deprecated *accumulating* `process` the
    /// same way JUCE does — that entry point adds into the output buffers, so
    /// they must be zeroed first. If neither pointer exists there is nothing to
    /// call; the outputs are left silent rather than filled with whatever the
    /// caller's scratch held.
    fn process(&mut self, buffer: &mut AudioBuffer<f32>) {
        if buffer.input_count() < self.info.inputs as usize {
            panic!("Too few inputs in AudioBuffer");
        }
        if buffer.output_count() < self.info.outputs as usize {
            panic!("Too few outputs in AudioBuffer");
        }
        let effect = self.get_effect();
        let samples = buffer.samples() as i32;
        unsafe {
            let replacing = (*effect).processReplacing;
            if self.can_replacing && !(replacing as *const u8).is_null() {
                replacing(
                    effect,
                    buffer.raw_inputs().as_ptr() as *const *const _,
                    buffer.raw_outputs().as_mut_ptr() as *mut *mut _,
                    samples,
                );
                return;
            }

            let accumulating = (*effect)._process;
            if (accumulating as *const u8).is_null() {
                error!(
                    "plugin '{}' installed neither processReplacing nor process; \
                     rendering silence",
                    self.info.name
                );
                zero_outputs(buffer);
                return;
            }

            warn!(
                "plugin '{}' does not support processReplacing; \
                 falling back to the deprecated accumulating process",
                self.info.name
            );
            // Accumulating mode *adds* to the outputs, so they must start at 0.
            zero_outputs(buffer);
            accumulating(
                effect,
                buffer.raw_inputs().as_ptr() as *const *const _,
                buffer.raw_outputs().as_mut_ptr() as *mut *mut _,
                samples,
            );
        }
    }

    /// Render one block in `f64`.
    ///
    /// There is no accumulating counterpart for double precision in VST2.4 —
    /// `processDoubleReplacing` is the only `f64` entry point — so an absent
    /// `effFlagsCanDoubleReplacing` or a null pointer leaves the caller to
    /// re-render through the `f32` path. Silence is returned rather than
    /// jumping to a null pointer; callers should consult
    /// `get_info().f64_precision` before choosing this path.
    fn process_f64(&mut self, buffer: &mut AudioBuffer<f64>) {
        if buffer.input_count() < self.info.inputs as usize {
            panic!("Too few inputs in AudioBuffer");
        }
        if buffer.output_count() < self.info.outputs as usize {
            panic!("Too few outputs in AudioBuffer");
        }
        let effect = self.get_effect();
        unsafe {
            let replacing = (*effect).processReplacingF64;
            if !self.can_double_replacing || (replacing as *const u8).is_null() {
                error!(
                    "plugin '{}' does not support f64 processing; rendering silence",
                    self.info.name
                );
                zero_outputs(buffer);
                return;
            }
            replacing(
                effect,
                buffer.raw_inputs().as_ptr() as *const *const _,
                buffer.raw_outputs().as_mut_ptr() as *mut *mut _,
                buffer.samples() as i32,
            );
        }
    }

    fn process_events(&mut self, events: &api::Events) {
        self.dispatch(
            plugin::OpCode::ProcessEvents,
            0,
            0,
            events as *const _ as *mut _,
            0.0,
        );
    }

    fn get_input_info(&self, input: i32) -> ChannelInfo {
        ChannelInfo::from(self.channel_properties(plugin::OpCode::GetInputInfo, input))
    }

    fn get_output_info(&self, output: i32) -> ChannelInfo {
        ChannelInfo::from(self.channel_properties(plugin::OpCode::GetOutputInfo, output))
    }

    fn get_parameter_object(&mut self) -> Arc<dyn PluginParameters> {
        Arc::clone(&self.params) as Arc<dyn PluginParameters>
    }

    fn get_editor(&mut self) -> Option<Box<dyn Editor>> {
        if self.is_editor_active {
            // An editor is already active, the caller should be using the active editor instead of
            // requesting for a new one.
            return None;
        }

        self.is_editor_active = true;
        Some(Box::new(EditorInstance {
            params: self.params.clone(),
            is_open: false,
        }))
    }
}

impl PluginParameters for PluginParametersInstance {
    fn change_preset(&self, preset: i32) {
        self.dispatch(
            plugin::OpCode::ChangePreset,
            0,
            preset as isize,
            ptr::null_mut(),
            0.0,
        );
    }

    fn get_preset_num(&self) -> i32 {
        self.opcode(plugin::OpCode::GetCurrentPresetNum) as i32
    }

    fn set_preset_name(&self, name: String) {
        self.write_string(plugin::OpCode::SetCurrentPresetName, 0, 0, &name, 0.0);
    }

    fn get_preset_name(&self, preset: i32) -> String {
        self.read_string_param(
            plugin::OpCode::GetPresetName,
            preset,
            0,
            0.0,
            MAX_PRESET_NAME_LEN,
        )
    }

    fn get_parameter_label(&self, index: i32) -> String {
        self.read_string_param(
            plugin::OpCode::GetParameterLabel,
            index,
            0,
            0.0,
            MAX_PARAM_STR_LEN,
        )
    }

    fn get_parameter_text(&self, index: i32) -> String {
        self.read_string_param(
            plugin::OpCode::GetParameterDisplay,
            index,
            0,
            0.0,
            MAX_PARAM_STR_LEN,
        )
    }

    fn get_parameter_name(&self, index: i32) -> String {
        self.read_string_param(
            plugin::OpCode::GetParameterName,
            index,
            0,
            0.0,
            MAX_PARAM_STR_LEN,
        )
    }

    fn get_parameter(&self, index: i32) -> f32 {
        unsafe { ((*self.get_effect()).getParameter)(self.get_effect(), index) }
    }

    fn set_parameter(&self, index: i32, value: f32) {
        unsafe { ((*self.get_effect()).setParameter)(self.get_effect(), index, value) }
    }

    fn can_be_automated(&self, index: i32) -> bool {
        self.dispatch(
            plugin::OpCode::CanBeAutomated,
            index,
            0,
            ptr::null_mut(),
            0.0,
        ) > 0
    }

    fn string_to_parameter(&self, index: i32, text: String) -> bool {
        self.write_string(plugin::OpCode::StringToParameter, index, 0, &text, 0.0) > 0
    }

    // TODO: Editor

    fn get_preset_data(&self) -> Vec<u8> {
        self.get_chunk(1 /*preset*/)
    }

    fn get_bank_data(&self) -> Vec<u8> {
        self.get_chunk(0 /*bank*/)
    }

    fn load_preset_data(&self, data: &[u8]) {
        self.dispatch(
            plugin::OpCode::SetData,
            1,
            data.len() as isize,
            data.as_ptr() as *mut c_void,
            0.0,
        );
    }

    fn load_bank_data(&self, data: &[u8]) {
        self.dispatch(
            plugin::OpCode::SetData,
            0,
            data.len() as isize,
            data.as_ptr() as *mut c_void,
            0.0,
        );
    }
}

/// Used for constructing `AudioBuffer` instances on the host.
///
/// This struct contains all necessary allocations for an `AudioBuffer` apart
/// from the actual sample arrays. This way, the inner processing loop can
/// be allocation free even if `AudioBuffer` instances are repeatedly created.
///
/// ```rust
/// # use vst_tutti::host::HostBuffer;
/// # use vst_tutti::plugin::Plugin;
/// # fn test<P: Plugin>(plugin: &mut P) {
/// let mut host_buffer: HostBuffer<f32> = HostBuffer::new(2, 2);
/// let inputs = vec![vec![0.0; 1000]; 2];
/// let mut outputs = vec![vec![0.0; 1000]; 2];
/// let mut audio_buffer = host_buffer.bind(&inputs, &mut outputs);
/// plugin.process(&mut audio_buffer);
/// # }
/// ```
pub struct HostBuffer<T: Float> {
    inputs: Vec<*const T>,
    outputs: Vec<*mut T>,
}

impl<T: Float> HostBuffer<T> {
    /// Create a `HostBuffer` for a given number of input and output channels.
    pub fn new(input_count: usize, output_count: usize) -> HostBuffer<T> {
        HostBuffer {
            inputs: vec![ptr::null(); input_count],
            outputs: vec![ptr::null_mut(); output_count],
        }
    }

    /// Create a `HostBuffer` for the number of input and output channels
    /// specified in an `Info` struct.
    pub fn from_info(info: &Info) -> HostBuffer<T> {
        HostBuffer::new(info.inputs as usize, info.outputs as usize)
    }

    /// Bind sample arrays to the `HostBuffer` to create an `AudioBuffer` to pass to a plugin.
    ///
    /// # Panics
    /// This function will panic if more inputs or outputs are supplied than the `HostBuffer`
    /// was created for, or if the sample arrays do not all have the same length.
    pub fn bind<'a, I, O>(
        &'a mut self,
        input_arrays: &[I],
        output_arrays: &mut [O],
    ) -> AudioBuffer<'a, T>
    where
        I: AsRef<[T]> + 'a,
        O: AsMut<[T]> + 'a,
    {
        // Check that number of desired inputs and outputs fit in allocation
        if input_arrays.len() > self.inputs.len() {
            panic!("Too many inputs for HostBuffer");
        }
        if output_arrays.len() > self.outputs.len() {
            panic!("Too many outputs for HostBuffer");
        }

        // Initialize raw pointers and find common length
        let mut length = None;
        for (i, input) in input_arrays.iter().map(|r| r.as_ref()).enumerate() {
            self.inputs[i] = input.as_ptr();
            match length {
                None => length = Some(input.len()),
                Some(old_length) => {
                    if input.len() != old_length {
                        panic!("Mismatching lengths of input arrays");
                    }
                }
            }
        }
        for (i, output) in output_arrays.iter_mut().map(|r| r.as_mut()).enumerate() {
            self.outputs[i] = output.as_mut_ptr();
            match length {
                None => length = Some(output.len()),
                Some(old_length) => {
                    if output.len() != old_length {
                        panic!("Mismatching lengths of output arrays");
                    }
                }
            }
        }
        let length = length.unwrap_or(0);

        // Construct AudioBuffer
        unsafe {
            AudioBuffer::from_raw(
                input_arrays.len(),
                output_arrays.len(),
                self.inputs.as_ptr(),
                self.outputs.as_mut_ptr(),
                length,
            )
        }
    }

    /// Number of input channels supported by this `HostBuffer`.
    pub fn input_count(&self) -> usize {
        self.inputs.len()
    }

    /// Number of output channels supported by this `HostBuffer`.
    pub fn output_count(&self) -> usize {
        self.outputs.len()
    }
}

/// HACK: a pointer to store the host so that it can be accessed from the `callback_wrapper`
/// function passed to the plugin.
///
/// When the plugin is being loaded, a `Box<Arc<T>>` is transmuted to a `*mut c_void` pointer
/// and placed here. When the plugin calls the callback during initialization, the host refers to
/// this pointer to get a handle to the Host. After initialization, this pointer is invalidated and
/// the host pointer is placed into a [reserved field] in the instance `AEffect` struct.
///
/// The issue with this approach is that if 2 plugins are simultaneously loaded with 2 different
/// host instances, this might fail as one host may receive a pointer to the other one. In practice
/// this is a rare situation as you normally won't have 2 separate host instances loading at once.
///
/// [reserved field]: ../api/struct.AEffect.html#structfield.reserved1
static mut LOAD_POINTER: *mut c_void = 0 as *mut c_void;

/// Function passed to plugin to handle dispatching host opcodes.
///
/// This is a C function pointer the plugin calls, including from its audio
/// thread inside `processReplacing`. Two properties are load-bearing:
///
/// * **No lock.** The host is reached through a plain `Arc<T>` (see
///   [`PluginLoader::load`]); a shared mutex here would make the audio thread
///   block on the GUI thread.
/// * **No unwinding.** Unwinding out of an `extern "C"` frame into the
///   plugin's C++ stack is undefined behaviour, so the whole dispatch is
///   wrapped in `catch_unwind` and a panic degrades to `0` — the VST2 "not
///   handled / not supported" answer for every host opcode.
extern "C" fn callback_wrapper<T: Host>(
    effect: *mut AEffect,
    opcode: i32,
    index: i32,
    value: isize,
    ptr: *mut c_void,
    opt: f32,
) -> isize {
    // AssertUnwindSafe: on a panic we return 0 and touch none of the captured
    // state again — the host is behind a shared reference and the raw pointers
    // are not re-read.
    let result = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        // If the effect pointer is not null and the host pointer is not null, the plugin has
        // already been initialized
        if !effect.is_null() && (*effect).reserved1 != 0 {
            let reserved = (*effect).reserved1 as *const Arc<T>;
            let host: &T = &*reserved;

            interfaces::host_dispatch(host, effect, opcode, index, value, ptr, opt)
        // In this case, the plugin is still undergoing initialization and so `LOAD_POINTER` is
        // dereferenced
        } else {
            // Used only during the plugin initialization
            let load_ptr = LOAD_POINTER as *const Arc<T>;
            if load_ptr.is_null() {
                return 0;
            }
            let host: &T = &*load_ptr;

            interfaces::host_dispatch(host, effect, opcode, index, value, ptr, opt)
        }
    }));

    match result {
        Ok(value) => value,
        Err(_) => {
            // Cannot let this cross back into the plugin's C++ frame.
            error!("host callback panicked (opcode {opcode}); reporting unhandled");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Supported;
    use crate::host::HostBuffer;

    /// A real plugin's `effCanDo` returns whatever its dispatcher left in the
    /// return slot — commonly the length of the queried string, or an
    /// uninitialised stack value. `Supported::from` used to return `None` for
    /// anything outside 1/0/-1 and the sole caller `.expect()`ed it, so probing
    /// such a plugin panicked *during load*, unwinding out of an `extern "C"`
    /// frame. It is now total.
    #[test]
    fn can_do_answers_outside_the_documented_set_do_not_panic() {
        assert_eq!(Supported::from(1), Supported::Yes);
        assert_eq!(Supported::from(0), Supported::Maybe);
        assert_eq!(Supported::from(-1), Supported::No);

        // The realistic garbage values.
        assert_eq!(Supported::from(14), Supported::Custom(14)); // strlen("receiveVstMidiEvent")-ish
        assert_eq!(Supported::from(-2), Supported::Custom(-2));
        assert_eq!(Supported::from(isize::MAX), Supported::Custom(isize::MAX));

        // And critically, none of them read as an affirmative "yes".
        for v in [2, 14, -2, 9999, isize::MIN, isize::MAX] {
            assert_ne!(Supported::from(v), Supported::Yes, "value {v}");
        }
    }

    /// The host must survive a plugin that sets bits VST2.4 does not define in
    /// `ChannelProperties::flags` — 29 of the 32 are unspecified, so any of
    /// them appearing is a plugin quirk, not a host error. This used to
    /// `.expect("Invalid bits in channel info")` and abort.
    #[test]
    fn undefined_channel_flag_bits_do_not_panic() {
        use crate::channels::ChannelInfo;

        let mut props: api::ChannelProperties = unsafe { MaybeUninit::zeroed().assume_init() };
        // ACTIVE plus a pile of bits the SDK says nothing about.
        props.flags = api::ChannelFlags::ACTIVE.bits() | 0x7FF0_0000;

        // The conversion is what used to panic; reaching this line is the test.
        let _info = ChannelInfo::from(props);
    }

    /// An all-zero `ChannelProperties` — what a plugin that ignores
    /// `effGetInputProperties` leaves behind now that we zero the buffer
    /// instead of handing over `MaybeUninit::uninit()` — must convert cleanly.
    #[test]
    fn zeroed_channel_properties_convert_cleanly() {
        use crate::channels::ChannelInfo;

        let props: api::ChannelProperties = unsafe { MaybeUninit::zeroed().assume_init() };
        let _info = ChannelInfo::from(props);
    }

    /// VST2-H5. `effGetChunk` used to be trusted blindly:
    /// `slice::from_raw_parts(ptr, len as usize)` with no null check and no
    /// sign check. This is the project-*save* path, so each of these is a real
    /// crash on a real user's save.
    #[test]
    fn get_chunk_rejects_unreadable_plugin_answers() {
        // `effFlagsProgramChunks` set but nothing to save yet: the plugin
        // returns 0 and never writes the out-pointer. `from_raw_parts(null, 0)`
        // is UB even at length zero — the pointer must always be non-null.
        assert!(unsafe { copy_chunk(ptr::null_mut(), 0) }.is_empty());

        // An error return. `-1 as usize` was `usize::MAX`.
        assert!(unsafe { copy_chunk(ptr::null_mut(), -1) }.is_empty());

        // A plugin that sized the chunk but failed to hand back the buffer.
        assert!(unsafe { copy_chunk(ptr::null_mut(), 4096) }.is_empty());

        // A non-null pointer with a negative length is still rejected — the
        // length is what would be cast, and it must never reach `as usize`.
        let mut data = [1u8, 2, 3, 4];
        assert!(unsafe { copy_chunk(data.as_mut_ptr(), -1) }.is_empty());

        // The good case still copies.
        assert_eq!(
            unsafe { copy_chunk(data.as_mut_ptr(), 4) },
            vec![1, 2, 3, 4]
        );
    }

    /// VST2-H4's fallback path clears the outputs before the accumulating
    /// `process` adds into them, and is also what a plugin with neither entry
    /// point gets. Whatever the caller's scratch held must not leak through.
    #[test]
    fn zero_outputs_silences_every_channel() {
        let mut host_buffer: HostBuffer<f32> = HostBuffer::new(2, 2);
        let inputs = vec![vec![1.0f32; 8]; 2];
        let mut outputs = vec![vec![0.5f32; 8]; 2];
        {
            let mut buffer = host_buffer.bind(&inputs, &mut outputs);
            zero_outputs(&mut buffer);
        }
        assert_eq!(outputs, vec![vec![0.0f32; 8]; 2]);
    }

    #[test]
    fn host_buffer() {
        const LENGTH: usize = 1_000_000;
        let mut host_buffer: HostBuffer<f32> = HostBuffer::new(2, 2);
        let input_left = vec![1.0; LENGTH];
        let input_right = vec![1.0; LENGTH];
        let mut output_left = vec![0.0; LENGTH];
        let mut output_right = vec![0.0; LENGTH];
        {
            let mut audio_buffer = {
                // Slices given to `bind` need not persist, but the sample arrays do.
                let inputs = [&input_left, &input_right];
                let mut outputs = [&mut output_left, &mut output_right];
                host_buffer.bind(&inputs, &mut outputs)
            };
            for (input, output) in audio_buffer.zip() {
                for (i, o) in input.iter().zip(output) {
                    *o = *i * 2.0;
                }
            }
        }
        assert_eq!(output_left, vec![2.0; LENGTH]);
        assert_eq!(output_right, vec![2.0; LENGTH]);
    }
}
