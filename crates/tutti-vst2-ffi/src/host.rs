//! Host specific structures.

use num_enum::{IntoPrimitive, TryFromPrimitive};
use num_traits::Float;

use libloading::Library;
use std::cell::{Cell, UnsafeCell};
use std::collections::BTreeSet;
use std::convert::TryFrom;
use std::error::Error;
use std::ffi::CString;
use std::marker::PhantomData;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::os::raw::c_void;
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::ThreadId;
use std::{fmt, ptr, slice};

use tutti_plugin_types::{MainThread, WindowHandle};

use crate::{
    api::{self, AEffect, ChunkError, PluginFlags, PluginMain, Supported, TimeInfo, consts::*},
    buffer::AudioBuffer,
    channels::ChannelInfo,
    editor::Rect,
    interfaces,
    plugin::{self, Category, Info},
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

    /// Handle incoming events from the plugin: what the host callback hands the host for
    /// `audioMasterProcessEvents`, as a view only the callback can make.
    fn process_plugin_events(&self, events: api::PluginEvents<'_>) {}

    /// Send events to the host. This is the plugin's call, on a `HostCallback`; the host
    /// side is [`process_plugin_events`](Self::process_plugin_events).
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

    /// Answer the plugin's `audioMasterCanDo` query for the capability named
    /// `can_do` (`"sendVstEvents"`, `"sendVstTimeInfo"`, ...).
    ///
    /// `Supported::Yes` for what the host does, `Supported::No` for what it
    /// declines, `Supported::Maybe` for a name it does not know: VST 2.4's
    /// `1`, `-1` and `0`. The default answers `Maybe` to everything.
    fn can_do(&self, can_do: &str) -> Supported {
        Supported::Maybe
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

    /// The entry point returned an `AEffect` that is not one: its `magic` is not
    /// `VstP`, or it has no dispatcher. Nothing was called on it.
    InvalidEffect,
}

impl fmt::Display for PluginLoadError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        use self::PluginLoadError::*;
        let description = match self {
            InvalidPath => "Could not open the requested path",
            NotAPlugin => "The given path does not contain a VST2.4 compatible library",
            InstanceFailed => "Failed to create a plugin instance",
            InvalidApiVersion => "The plugin API version is not compatible with this library",
            InvalidEffect => "The plugin entry point returned an invalid AEffect",
        };
        write!(f, "{}", description)
    }
}

impl Error for PluginLoadError {}

/// Wrapper for an externally loaded VST plugin.
///
/// The only functionality this struct provides is loading plugins, which can be done via the
/// [`load`](#method.load) method.
#[derive(Debug)]
pub struct PluginLoader<T: Host> {
    main: PluginMain,
    lib: Arc<Library>,
    host: Arc<T>,
}

/// What a plugin answered for its MIDI channel counts.
///
/// `None` on a field means the plugin did not answer that opcode — which is
/// the common case, and is **not** the same as answering zero. See
/// [`PluginInstance::read_midi_channels`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MidiChannelCounts {
    /// `effGetNumMidiInputChannels`, 1..=15 when answered.
    pub inputs: Option<u8>,
    /// `effGetNumMidiOutputChannels`, 1..=15 when answered.
    pub outputs: Option<u8>,
}

/// A loaded VST plugin, and the main-thread handle on it.
///
/// This is the one owner of the plugin. It sends `effClose` when it drops, and everything
/// that is not a render entry is its method. The other half is the [`RenderHandle`]
/// ([`render_handle`](Self::render_handle)): the audio thread's entries, which share the
/// plugin's core but do not own the plugin, so dropping one never closes it, and an entry
/// called after the close is refused without touching the `AEffect` the plugin freed.
///
/// # Thread rules
///
/// The rule is in the types, as in the other formats (`tutti_plugin_types::MainThread`):
///
/// - An entry that VST2 puts on the main thread takes a `&MainThread`, the proof that the
///   caller is on it: `effOpen`, `effMainsChanged`, the sample rate and block size, state
///   (`effGetChunk`, `effSetChunk`), programs, bypass, the editor, the MIDI program
///   queries, `effCanDo`, `effGetTailSize`. Nothing is checked at the call.
/// - `getParameter` and `setParameter` are the only dispatcher-free entries VST2 lets any thread
///   call beside `process`: they take no token and no gate.
/// - **Named exceptions: token-free, serialised.** The in-process backend of `tutti-plugin`
///   asks for a parameter's name, label and display, its properties, whether it can be
///   automated, and parses a string into it (`get_parameter_name`, `get_parameter_label`,
///   `get_parameter_text`, `parameter_properties`, `can_be_automated`, `string_to_parameter`),
///   and for the state (`chunk_off_main_thread`), off the main thread, behind its own lock.
///   VST2 does not allow these beside `process` (they are dispatcher calls), so each holds
///   the audio gate for the call, as the exclusive calls do: it waits for a render in flight
///   and a render that starts meanwhile is refused. They take no token only because the
///   backend has none; narrowing them to the main thread waits for the session to give it a
///   route there.
/// - Plain `AEffect` fields (the pin counts, `initialDelay`, `numPrograms`) are read
///   without a token; they are not calls into the plugin.
///
/// The instance is `Send`, so a host can load on one thread and hand it to another; `Send`
/// permits moving it and is not a claim that any thread may call it. It is not `Sync`.
///
/// **A drop has no token**, so the close is guarded another way: the instance records the
/// thread it was made on (the one that held the `&MainThread` it was made with), and drops
/// the plugin properly only there. Dropped anywhere else it **leaks** the plugin, never
/// closing it and never freeing its core: a plugin that closes on the wrong thread may not
/// survive, and the live [`RenderHandle`] may still be rendering. The host crate that owns
/// the instance drops it on the control side.
///
/// The drop on the right thread is: close the editor if open, `effStopProcess` if
/// processing, `effMainsChanged(0)` if resumed, then `effClose`, in that order and under the
/// audio gate, so an audio entry in flight finishes first and later ones are refused.
///
/// ```compile_fail,E0061
/// # use tutti_vst2_ffi::host::PluginInstance;
/// // A main-thread entry needs the proof: there is no token-free `resume`.
/// fn go(instance: &mut PluginInstance) { instance.resume(); }
/// ```
///
/// ```compile_fail,E0277
/// # use tutti_vst2_ffi::host::PluginInstance;
/// fn needs_sync<T: Sync>() {}
/// needs_sync::<PluginInstance>();
/// ```
///
/// Mutation (run): give `resume` back its token-free signature, or drop the `PhantomData`
/// marker that makes the instance `!Sync`: the matching doctest compiles and fails.
#[allow(dead_code)] // To keep `lib` around.
#[derive(Debug)]
pub struct PluginInstance {
    core: Arc<Core>,
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
    /// The thread this instance was made on, under a `&MainThread`: the only one it closes on.
    main_thread: ThreadId,
    /// Whether the last `effMainsChanged` dispatched carried `value=1`.
    ///
    /// Tracked because `effMainsChanged` is not documented as idempotent and real
    /// plugins reallocate rate-dependent buffers on every `resume(1)`. The drop reads it so
    /// a plugin that is not resumed is not suspended a second time.
    resumed: bool,
    /// Whether the editor is open (`effEditOpen` answered, no `effEditClose` since).
    editor_open: bool,
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
    /// `effFlagsHasEditor` in `AEffect::flags` — whether the plugin publishes a
    /// GUI at all.
    has_editor: bool,
    /// `&self` entries with a token are main-thread calls; `!Sync` keeps the handle to the
    /// thread that holds it.
    _not_sync: PhantomData<Cell<()>>,
}

/// An owned `Box<Arc<T>>` with `T` erased, freed on drop.
///
/// `AEffect::reserved1` points into this box, so a plugin must not call the host after
/// `effClose`: the box is freed once the instance drops, and a later callback would read freed
/// memory.
struct HostRef {
    ptr: *mut c_void,
    free: unsafe fn(*mut c_void),
}

impl fmt::Debug for HostRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HostRef")
            .field("ptr", &self.ptr)
            .finish_non_exhaustive()
    }
}

// SAFETY: `HostRef::new` only accepts `T: Send + Sync`. Dropping runs `T`'s destructor and
// decrements a shared count, which is sound from any thread for such a `T`.
unsafe impl Send for HostRef {}
// SAFETY: as for `Send`; the pointer is never shared out through `&HostRef`.
unsafe impl Sync for HostRef {}

impl HostRef {
    /// Boxes `host`; the box's address is what `AEffect::reserved1` carries.
    fn new<T: Send + Sync + 'static>(host: Arc<T>) -> HostRef {
        unsafe fn free<T>(ptr: *mut c_void) {
            // SAFETY: `ptr` came from `Box::into_raw` of a `Box<Arc<T>>` in `HostRef::new`.
            drop(unsafe { Box::from_raw(ptr as *mut Arc<T>) });
        }
        HostRef {
            ptr: Box::into_raw(Box::new(host)) as *mut c_void,
            free: free::<T>,
        }
    }
}

impl Drop for HostRef {
    fn drop(&mut self) {
        // SAFETY: `free` is the monomorphization that matches `ptr`, and runs once.
        unsafe { (self.free)(self.ptr) };
    }
}

/// The core of one live plugin instance: its `AEffect`, the host box that
/// `AEffect::reserved1` points at, and the gate between the audio entries and the close.
///
/// The [`PluginInstance`] and the [`RenderHandle`] hold an `Arc` of this. The core owns
/// no plugin: `effClose` is the instance's drop, under [`gate`](Self::gate), which also
/// marks the gate closed. A render handle that outlives the instance therefore holds a
/// core whose `AEffect` pointer must not be followed, and does not: its every entry
/// takes the gate first and finds it closed.
struct Core {
    effect: UnsafeCell<*mut AEffect>,
    /// The `Box<Arc<T>>` that `AEffect::reserved1` points at. A field, so it drops after
    /// the close, the last call that can reach the host.
    _host_ref: Option<HostRef>,
    /// Blocks a render entry refused because the plugin's pin counts grew past the
    /// buffer (S7). Counted on the audio thread, read and reported by the control side
    /// (`PluginInstance::pin_refusals`); `Relaxed`, nothing is published through it.
    pin_refusals: AtomicU64,
    /// Whether the control side has logged `pin_refusals` going nonzero.
    pin_refusals_logged: AtomicBool,
    /// Whether `effStartProcess` is the plugin's last word. Written under the gate, by the
    /// render handle that starts and stops, and by the instance's drop that stops it when
    /// no one else did; read anywhere.
    processing: AtomicBool,
    /// Held by an audio entry for the whole call (`try_lock`: it never waits), and by the
    /// instance's drop for the close (`lock`: it waits for the call in flight). `true`
    /// inside means the plugin is closed.
    gate: Mutex<bool>,
    /// The thread the instance was made on: the one the plugin is closed on, and the one a
    /// core the audio side outlives it on is freed on.
    main_thread: ThreadId,
}

impl fmt::Debug for Core {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Core").finish_non_exhaustive()
    }
}

impl Core {
    /// A core over `effect`, which must be a live `AEffect` that the plugin has not
    /// closed. Nothing here closes it; the instance does.
    fn new(effect: *mut AEffect, host_ref: Option<HostRef>, main_thread: ThreadId) -> Arc<Self> {
        let core = Arc::new(Core {
            effect: UnsafeCell::new(effect),
            _host_ref: host_ref,
            pin_refusals: AtomicU64::new(0),
            pin_refusals_logged: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            gate: Mutex::new(false),
            main_thread,
        });
        // Warm the gate here, on the control side. On macOS (and the other platforms whose
        // `std::sync::Mutex` is a `pthread_mutex_t`) the mutex allocates its 64-byte
        // `pthread_mutex_t` lazily, on the first `lock` or `try_lock`; left alone, that first
        // touch would be the first audio entry's `try_lock`, an allocation on the render path
        // (the no-alloc gate of `a_callback_from_a_render_takes_no_lock_during_a_load` aborts
        // on it). Linux and Windows use a futex or SRW lock that never allocates. One
        // uncontended lock now means no later `lock`/`try_lock` on any thread allocates.
        drop(
            core.gate
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        core
    }
}

// SAFETY: what the type itself holds is a pointer cell that is written once at
// construction and only read afterwards, atomics, a mutex, and the host box, which is
// `Send + Sync` (`HostRef`). The `AEffect` it points at is the plugin's, live until the
// instance's close, and only followed by a holder of the gate that has seen it open, or
// by the instance itself (the one holder that closes). Moving the core to another thread
// lets a `RenderHandle` there call the audio entries, which is what VST2 expects of the
// audio thread.
unsafe impl Send for Core {}
// SAFETY: as for `Send`; shared access to the cell is reads of an immutable pointer, and the
// rest is atomics and a mutex. `&Core` gives no entry beyond the private dispatch the
// instance and the render handle make under their own rules (the token, the gate).
unsafe impl Sync for Core {}

/// What a close needs of an instance, once the instance itself is gone: the core, and the two
/// flags that say which opcodes the sequence sends. The instance's drop runs it on the spot
/// when it is on the right thread; off it, the sequence is handed to the main thread
/// ([`defer_to_main`](tutti_plugin_types::defer_to_main)) instead of being skipped.
struct Closer {
    core: Arc<Core>,
    editor_open: bool,
    resumed: bool,
}

impl Closer {
    /// Sends the opcodes of a close, once, on the main thread; see [`PluginInstance`]'s docs.
    fn close(self) {
        use plugin::OpCode as op;

        // The editor first: some plugins read audio state during editor teardown (to flush
        // meter buffers).
        if self.editor_open {
            self.core
                .dispatch(op::EditorClose, 0, 0, ptr::null_mut(), 0.0);
        }
        // Waits for an audio entry in flight; after this none starts.
        let mut closed = self
            .core
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // `effStopProcess`, which the SDK puts before the suspend, is the render handle's;
        // when no one stopped it, it is stopped here, once.
        if self.core.processing.swap(false, Ordering::AcqRel) {
            self.core
                .dispatch(op::StopProcess, 0, 0, ptr::null_mut(), 0.0);
        }
        // A plugin already suspended (a rejoin left it so) would see a second
        // `effMainsChanged(0)` with no start before it.
        if self.resumed {
            self.core
                .dispatch(op::StateChanged, 0, 0, ptr::null_mut(), 0.0);
        }
        // `effClose`. The plugin frees its own `AEffect` in response, so this must happen
        // exactly once, and it *must* happen: a licensed plugin releases its session seat
        // here, and skipping it leaks one live instance (and one licence) per A/B of a slot.
        *closed = true;
        self.core.dispatch(op::Shutdown, 0, 0, ptr::null_mut(), 0.0);
        // After `effClose`: the plugin may call the host while closing.
        unadopt(self.core.get_effect());
        // The module is never `dlclose`d: `PluginInstance::lib` is `ManuallyDrop`.
    }
}

/// How many instances were dropped off the thread they were made on, since the process began.
/// Each one's close was handed to the main thread; one still waiting for a tick is the plugin
/// not yet closed.
static OFF_THREAD_DROPS: AtomicU64 = AtomicU64::new(0);

/// How many [`PluginInstance`]s have been dropped off their own thread so far: each had its
/// close handed to the main thread ([`tutti_plugin_types::run_deferred`], which the session's
/// `Phase::Collect` calls). A nonzero count is worth a look, since the owner should have
/// dropped it on the main thread.
pub fn off_thread_drops() -> u64 {
    OFF_THREAD_DROPS.load(Ordering::Relaxed)
}

impl Drop for PluginInstance {
    fn drop(&mut self) {
        let closer = Closer {
            core: Arc::clone(&self.core),
            editor_open: self.editor_open,
            resumed: self.resumed,
        };
        if std::thread::current().id() == self.main_thread {
            closer.close();
            return;
        }
        // A drop has no token. Off the main thread `effClose` and the rest would run on the
        // wrong thread, which a plugin may not survive, and skipping them leaks the plugin and
        // its licence seat. So the sequence goes to the main thread, to run at its next
        // `run_deferred` (the session's collect tick); the core stays alive in the closure,
        // so a live `RenderHandle` keeps working until then. This is a control thread (never
        // the audio thread, which owns no instance), so a log line is fine.
        let count = OFF_THREAD_DROPS.fetch_add(1, Ordering::Relaxed) + 1;
        warn!(
            "a VST2 plugin instance was dropped off its own thread ({count} so far); its close \
             is handed to the main thread"
        );
        tutti_plugin_types::defer_to_main(move |_main| closer.close());
    }
}

impl<T: Host + Send + Sync + 'static> PluginLoader<T> {
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
    /// # Errors
    ///
    /// [`PluginLoadError::InvalidPath`] if the library at `path` cannot be
    /// opened, and [`PluginLoadError::NotAPlugin`] if it has no `VSTPluginMain`
    /// entry point.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::path::Path;
    /// # use std::sync::Arc;
    /// # use tutti_vst2_ffi::host::{Host, PluginLoader};
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
    /// let main = tutti_plugin_types::MainThread::claim().unwrap();
    /// let instance = plugin.instance(&main).unwrap();
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
        //
        // SAFETY: loading a library runs its initialisers, which is inherent to hosting a
        // plugin and trusts the file at `path`. `lib.get` asks for `VSTPluginMain` as the
        // `PluginMain` fn type; that the exported symbol has that signature is the VST2 entry
        // point ABI and cannot be checked. The copied fn pointer stays valid because `lib` is
        // stored beside it in the loader (and `PluginInstance` never unloads it).
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
    ///
    /// While `main` runs, this thread's [`LOAD_HOST`] points at `self.host`, so the plugin's
    /// callbacks made during initialization (on this thread) reach this loader's host.
    fn call_main(&mut self) -> *mut AEffect {
        // Restores the previous value on exit, so a plugin whose `main` loads another plugin on
        // the same thread nests correctly.
        struct Restore(*mut c_void);
        impl Drop for Restore {
            fn drop(&mut self) {
                LOAD_HOST.set(self.0);
            }
        }

        // `&self.host` outlives the call below: `self` is borrowed for all of `call_main`.
        let _restore = Restore(LOAD_HOST.replace(&self.host as *const Arc<T> as *mut c_void));
        (self.main)(callback_wrapper::<T>)
    }

    /// Try to create an instance of this VST plugin.
    ///
    /// If the instance is successfully created, a [`PluginInstance`] is returned, bound to
    /// the thread `main` proves: the one it closes the plugin on.
    ///
    /// # Errors
    ///
    /// [`PluginLoadError::InstanceFailed`] if the plugin's entry point returns
    /// no instance, and [`PluginLoadError::InvalidApiVersion`] if the instance
    /// speaks a VST API version this library does not support.
    pub fn instance(&mut self, main: &MainThread) -> Result<PluginInstance, PluginLoadError> {
        // A load is in progress from before `main` until this returns, so a callback made
        // by any thread of the plugin in that window is told apart from a steady-state one
        // (`callback_wrapper`).
        let _loading = Loading::begin();
        // Call the plugin main function. This also passes the plugin main function as the closure
        // could not return an error if the symbol wasn't found
        let effect = self.call_main();

        if effect.is_null() {
            return Err(PluginLoadError::InstanceFailed);
        }

        // SAFETY: `effect` is non-null and was just returned by the plugin's `main`, so
        // VST2 has it point at the plugin's `AEffect`; `magic` and `dispatcher` are plain
        // fields read before anything is called through it.
        let (magic, dispatcher) = unsafe { ((*effect).magic, (*effect).dispatcher) };
        if magic != VST_MAGIC || dispatcher.is_none() {
            // Not an `AEffect` the host can drive, so it is not called at all, not even
            // with `effClose`: the pointer is left alone.
            warn!("plugin entry point returned an invalid AEffect (magic {magic:#x})");
            return Err(PluginLoadError::InvalidEffect);
        }

        // The core owns the box, so it is freed after `effClose`; its heap address is stable.
        let host_ref = HostRef::new(Arc::clone(&self.host));
        // Adopted before `reserved1` is written, and un-adopted by the core's drop after
        // `effClose`: while the address is in the registry, `reserved1` is ours.
        adopt(effect);
        // SAFETY: `effect` is non-null and was just returned by the plugin's `main`; nothing
        // else reads `reserved1` until the plugin calls back with this effect pointer, and a
        // callback that does is let through by `adopt` above.
        unsafe { (*effect).reserved1 = host_ref.ptr as isize };

        let instance = PluginInstance::new(effect, Arc::clone(&self.lib), Some(host_ref), main);

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
    /// Re-read the counts and delay [`Info`] snapshots from the
    /// live `AEffect`: `numInputs`, `numOutputs`, `numParams`, `numPrograms`
    /// and `initialDelay`. The snapshot is taken before `effOpen`, so
    /// [`init`](Self::init) calls this once it returns; a host calls it again
    /// after `audioMasterIOChanged`, which is the plugin saying the pin counts
    /// moved.
    pub fn refresh_effect_fields(&mut self) {
        // SAFETY: `self.core` owns the `*mut AEffect` for this instance's
        // lifetime; these are plain `i32` fields, not calls into the plugin.
        let effect = unsafe { &*self.core.get_effect() };
        self.info.inputs = effect.numInputs;
        self.info.outputs = effect.numOutputs;
        self.info.parameters = effect.numParams;
        self.info.presets = effect.numPrograms;
        self.info.initial_delay = effect.initialDelay;
    }

    /// Read `AEffect::numPrograms` as it stands *now*.
    ///
    /// [`get_info`](Self::get_info) returns the snapshot taken in
    /// `new`, before `effOpen`. A **shell** plugin — one bundle
    /// exposing many effects — picks which effect it is during init, and its
    /// program count is that effect's, not the shell's. Reading the snapshot
    /// gives the count from before the choice.
    ///
    /// Same live-field reasoning as
    /// [`read_initial_delay`](Self::read_initial_delay); `numPrograms` is a
    /// plain `i32` on the `AEffect`, not a call into the plugin.
    pub fn read_num_programs(&self) -> i32 {
        // SAFETY: `self.core` owns the `*mut AEffect` for this instance's
        // lifetime; `numPrograms` is a plain `i32` field, so there is no
        // re-entrancy or thread-affinity concern.
        unsafe { (*self.core.get_effect()).numPrograms }
    }

    /// Bracket a preset change with `effBeginSetProgram`(67) /
    /// `effEndSetProgram`(68), running `change` between them.
    ///
    /// A preset switch moves many parameters at once. Unbracketed, each one
    /// reaches the host as an individual `audioMasterAutomate` edit, so a
    /// switch reads as a storm of automation writes rather than one atomic
    /// event — and this host has that callback wired and draining, so the storm
    /// is real rather than hypothetical.
    ///
    /// Both opcodes are optional and their return is not load-bearing: an
    /// unimplemented one falls through the dispatcher and the change still
    /// happens. The bracket is advisory, so there is nothing here to report.
    pub fn with_preset_bracket<R>(
        &mut self,
        _main: &MainThread,
        change: impl FnOnce(&mut Self) -> R,
    ) -> R {
        self.opcode(plugin::OpCode::BeginSetPreset);
        let result = change(self);
        self.opcode(plugin::OpCode::EndSetPreset);
        result
    }

    /// Ask the plugin to enter or leave *soft* bypass, via `effSetBypass`(44).
    ///
    /// Returns `false` when the plugin refuses or does not implement the
    /// opcode — the two are indistinguishable, since an unimplemented opcode
    /// falls through the dispatcher returning 0, which is also "no".
    ///
    /// Soft bypass is the plugin's own passthrough, and it is what a host wants
    /// where one exists: the plugin crossfades and flushes its tail instead of
    /// cutting mid-reverb. It is **not** a substitute for the host's own mute,
    /// it is an alternative to it — a refusal means the host must do the
    /// bypass itself, which is why the answer is returned rather than dropped.
    /// A caller that discards it leaves the plugin audibly processing while the
    /// UI shows it bypassed.
    ///
    /// Check [`can_do(CanDo::Bypass)`](Self::can_do) first. A plugin that
    /// does not advertise `"bypass"` may still return non-zero here, and the
    /// advertisement is the documented contract.
    pub fn set_bypass(&mut self, _main: &MainThread, bypass: bool) -> bool {
        self.dispatch(
            plugin::OpCode::SoftBypass,
            0,
            isize::from(bypass),
            ptr::null_mut(),
            0.0,
        ) != 0
    }

    /// Read `AEffect::initialDelay` as it stands *now*.
    ///
    /// [`get_info`](Self::get_info) returns a clone of the snapshot taken in
    /// `new`, which runs before `effOpen`, `effSetSampleRate` and
    /// `effMainsChanged`. Plugins routinely set their latency during those —
    /// a linear-phase EQ does not know its filter length until it knows the
    /// sample rate — so the snapshot's `initial_delay` is a pre-init value and
    /// is usually 0 for exactly the plugins that have latency.
    ///
    /// This reads the field back off the live `AEffect` instead, so a host can
    /// re-ask after the init sequence and after any state change that lets a
    /// plugin re-declare.
    pub fn read_initial_delay(&self) -> i32 {
        // SAFETY: `self.core` owns the `*mut AEffect` for this instance's
        // lifetime; `initialDelay` is a plain `i32` field, not a call into the
        // plugin, so there is no re-entrancy or thread-affinity concern.
        unsafe { (*self.core.get_effect()).initialDelay }
    }

    /// Ask the plugin for its ring-out, via `effGetTailSize`.
    ///
    /// **The raw answer, undecoded**, because VST 2.4 inverts the convention
    /// every other format uses: `0` means "no tail *information*, host
    /// decides", `1` means "no tail at all", and anything larger is a sample
    /// count. Returning the wire value keeps that decision with the caller —
    /// mapping it here would have to pick a vocabulary, and the two candidate
    /// zeros mean opposite things.
    ///
    /// Read live rather than from the `get_info` snapshot, for the same reason
    /// [`read_initial_delay`](Self::read_initial_delay) is: a plugin may not
    /// know its tail until it knows its sample rate.
    pub fn read_tail_size(&self, _main: &MainThread) -> isize {
        self.dispatch(plugin::OpCode::GetTailSize, 0, 0, std::ptr::null_mut(), 0.0)
    }

    /// Ask the plugin how many MIDI channels it uses, via
    /// `effGetNumMidiInputChannels` / `effGetNumMidiOutputChannels`.
    ///
    /// `None` when the plugin does not implement the opcode. Both opcodes
    /// return their count as the dispatcher's `isize` return value, and an
    /// unimplemented opcode falls through the plugin's dispatcher returning 0 —
    /// so a bare `0` cannot be told apart from "I use no MIDI channels". The
    /// spec bounds a real answer at 1..=15, which is what makes the two
    /// separable at all: anything outside that range is an absent answer, not a
    /// count. A host must therefore treat `None` as "unknown", never as zero.
    ///
    /// Not folded into [`plugin::Info`] because that snapshot is taken in
    /// `new`, before `effOpen`, and a plugin may not know its MIDI
    /// configuration until it is initialised — the same trap
    /// [`read_initial_delay`](Self::read_initial_delay) documents for latency.
    pub fn read_midi_channels(&self, _main: &MainThread) -> MidiChannelCounts {
        fn ask(this: &PluginInstance, opcode: plugin::OpCode) -> Option<u8> {
            // VST 2.4 documents the valid answer as 1..=15. Out of that range —
            // including the 0 an unimplemented opcode returns — means the
            // plugin did not answer.
            u8::try_from(this.opcode(opcode))
                .ok()
                .filter(|n| (1..=15).contains(n))
        }

        MidiChannelCounts {
            inputs: ask(self, plugin::OpCode::GetNumMidiInputs),
            outputs: ask(self, plugin::OpCode::GetNumMidiOutputs),
        }
    }

    fn entry(&self) -> RenderEntry {
        RenderEntry {
            effect: self.core.get_effect(),
            inputs: self.info.inputs.max(0) as usize,
            outputs: self.info.outputs.max(0) as usize,
            can_replacing: self.can_replacing,
            can_double_replacing: self.can_double_replacing,
        }
    }

    /// The audio-thread entries of this plugin, with the pin counts read from the
    /// `AEffect` now: a plugin settles them in `effOpen` and may change them
    /// later, and a block must carry what it has at this moment.
    ///
    /// The handle shares the instance's core and does not own the plugin: dropping it never
    /// closes the plugin, and dropping the instance while the handle lives closes it (on the
    /// instance's thread) and leaves the handle refusing every entry.
    pub fn render_handle(&self, _main: &MainThread) -> RenderHandle {
        let (inputs, outputs) = self.read_pin_counts();
        RenderHandle {
            entry: RenderEntry {
                inputs: inputs.max(0) as usize,
                outputs: outputs.max(0) as usize,
                ..self.entry()
            },
            name: Arc::from(self.info.name.as_str()),
            core: Arc::clone(&self.core),
        }
    }

    /// Read `AEffect::numInputs` and `numOutputs` as they stand *now*, as
    /// [`read_initial_delay`](Self::read_initial_delay) reads its field.
    pub fn read_pin_counts(&self) -> (i32, i32) {
        // SAFETY: `self.core` owns the `*mut AEffect` for this instance's
        // lifetime; these are plain `i32` fields, not calls into the plugin.
        let effect = unsafe { &*self.core.get_effect() };
        (effect.numInputs, effect.numOutputs)
    }

    /// How many blocks have been rendered as silence because the plugin raised its pin
    /// counts past the buffer it was activated with.
    ///
    /// **S7 stays open:** the plugin renders silence until it is re-activated, which
    /// the host will do on `audioMasterIOChanged` (step 7 of the VST2 fix plan; today
    /// `RESTARTS` is `NONE`). The counter is for the control side to report once; the
    /// audio thread never logs.
    pub fn pin_refusals(&self) -> u64 {
        self.core.pin_refusals.load(Ordering::Relaxed)
    }

    /// [`pin_refusals`](Self::pin_refusals), and the first time it is nonzero, one
    /// warning. For the control side's tick; never call it from the audio thread.
    pub fn report_pin_refusals(&self) -> u64 {
        let refused = self.pin_refusals();
        if refused > 0 && !self.core.pin_refusals_logged.swap(true, Ordering::Relaxed) {
            warn!(
                "plugin '{}' now declares more pins than it was activated with; it renders \
                 silence until it is re-activated ({refused} blocks so far)",
                self.info.name
            );
        }
        refused
    }

    /// Read `AEffect::numParams` as it stands *now*.
    pub fn read_num_params(&self) -> i32 {
        // SAFETY: as for `read_pin_counts`.
        unsafe { (*self.core.get_effect()).numParams }
    }

    fn new(
        effect: *mut AEffect,
        lib: Arc<Library>,
        host_ref: Option<HostRef>,
        _main: &MainThread,
    ) -> PluginInstance {
        use plugin::OpCode as op;

        let main_thread = std::thread::current().id();
        let core = Core::new(effect, host_ref, main_thread);
        let mut plug = PluginInstance {
            core,
            lib: ManuallyDrop::new(lib),
            info: Default::default(),
            main_thread,
            resumed: false,
            editor_open: false,
            can_replacing: false,
            can_double_replacing: false,
            has_editor: false,
            _not_sync: PhantomData,
        };

        // SAFETY: `effect` is the non-null `AEffect*` just returned by the plugin's `main`
        // (`PluginLoader::instance` rejects null before calling `new`), and the plugin keeps
        // it live until `effClose`, which only runs when the core (`plug.params`) drops.
        unsafe {
            let effect: &AEffect = &*effect;
            let flags = PluginFlags::from_bits_truncate(effect.flags);

            plug.can_replacing = flags.intersects(PluginFlags::CAN_REPLACING);
            plug.can_double_replacing = flags.intersects(PluginFlags::CAN_DOUBLE_REPLACING);
            plug.has_editor = flags.intersects(PluginFlags::HAS_EDITOR);

            // `dispatch` returns `isize`, but `effGetPlugCategory`'s return is
            // an `i32` in the ABI, so the upper half of a 64-bit answer is not
            // the plugin's. Saturating rather than truncating: a value that does
            // not fit was never a category code, and truncation could land it on
            // a real one.
            let category_code = plug.opcode(op::GetCategory).try_into().unwrap_or(i32::MAX);

            // `effGetEffectName` is the plugin's own name for itself;
            // `effGetProductString` is the product it ships in, which for a
            // bundled suite is one name shared by every plugin in it. Asking
            // only for the product string therefore collapses a whole suite to
            // a single label.
            //
            // Both opcodes are optional and neither reports failure — an
            // unimplemented one falls through the dispatcher without touching
            // the buffer, which stays zero-filled. So an empty string is the
            // only available "did not answer", and the fallback is driven by
            // it. A caller that gets neither is left with an empty name and
            // supplies its own (the file stem); that decision is not this
            // layer's to make.
            let effect_name = plug.read_string(op::GetEffectName, MAX_EFFECT_NAME_LEN);
            let name = if effect_name.is_empty() {
                plug.read_string(op::GetProductName, MAX_PRODUCT_STR_LEN)
            } else {
                effect_name
            };

            plug.info = Info {
                name,
                vendor: plug.read_string(op::GetVendorName, MAX_VENDOR_STR_LEN),

                presets: effect.numPrograms,
                parameters: effect.numParams,
                inputs: effect.numInputs,
                outputs: effect.numOutputs,

                midi_inputs: 0,
                midi_outputs: 0,

                unique_id: effect.uniqueId,
                version: effect.version,

                category: Category::try_from(category_code as isize).unwrap_or(Category::Unknown),
                category_code,

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
        // valid discriminants are -2..=28; anything else is not a value of the
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
        //
        // SAFETY: every byte was zeroed before the plugin ran, so none is uninitialised; the
        // only field with a restricted bit pattern (`arrangement_type`) was clamped to a valid
        // discriminant above, and the rest are integers and byte arrays.
        unsafe { props.assume_init() }
    }

    /// Query `effGetParameterProperties` for one parameter.
    ///
    /// `None` when the plugin does not implement the opcode — which is the
    /// common case, not an error. Measured against three shipping plugins
    /// (TAL-NoiseMaker, TAL-Reverb-4, TDR Nova): all three answer `0` for every
    /// parameter, so a host must treat absence as normal and fall back to the
    /// name/label pair.
    ///
    /// Absence is detected by the *return value*, never by inspecting the
    /// buffer. An unimplemented opcode falls through the plugin's dispatcher
    /// without writing anything, so the buffer still holds the zeros we put
    /// there — indistinguishable from a plugin that really means
    /// "range 0..0, no category".
    pub fn parameter_properties(&self, index: i32) -> Option<api::ParameterProperties> {
        // SAFETY: `ParameterProperties` is `#[repr(C)]` POD — three `f32`s,
        // `i32`/`i16` scalars and byte arrays. Every field's all-zero bit
        // pattern is a valid value of its type (no enums, no references, no
        // `NonZero`), so a zeroed struct is fully initialised before the plugin
        // sees it and remains valid whether or not the plugin writes.
        let mut props: MaybeUninit<api::ParameterProperties> = MaybeUninit::zeroed();

        let supported = self.exclusive(|| {
            self.dispatch(
                plugin::OpCode::GetParamInfo,
                index,
                0,
                props.as_mut_ptr() as *mut c_void,
                0.0,
            )
        });

        // VST 2.4 specifies "1 if supported". Compare against 1 rather than
        // testing `!= 0`: plugins do return negative values from these optional
        // opcodes (a measured plugin answers `-1` to `effGetCurrentMidiProgram`),
        // and `-1` means "no", not "yes".
        if supported != 1 {
            return None;
        }

        // SAFETY: zeroed above, so initialised regardless of what the plugin
        // did; the plugin may only have overwritten bytes in place.
        Some(unsafe { props.assume_init() })
    }

    /// Query `effGetMidiProgramName` for the program at `program_index` on
    /// `channel`.
    ///
    /// Returns the filled struct plus the plugin's return value, which for this
    /// opcode is the *number of programs it services* — not a boolean. `0`
    /// means unsupported, so `None` covers both "no such opcode" and "no
    /// programs".
    ///
    /// The host writes `this_program_index` before dispatch: it is the question,
    /// not part of the answer. Leaving it zero asks about program 0 every time,
    /// which reads as "the plugin reports the same name for every program".
    pub fn midi_program_name(
        &self,
        _main: &MainThread,
        channel: i32,
        program_index: i32,
    ) -> Option<(api::MidiProgramName, i32)> {
        // SAFETY: as `parameter_properties` — `#[repr(C)]` POD whose all-zero
        // bit pattern is valid for every field.
        let mut name: MaybeUninit<api::MidiProgramName> = MaybeUninit::zeroed();

        // Write the query into the struct before handing it over.
        //
        // SAFETY: `name` is zeroed and therefore initialised; the offset comes
        // from `addr_of_mut!` on that same allocation.
        unsafe {
            ptr::addr_of_mut!((*name.as_mut_ptr()).this_program_index).write(program_index);
        }

        let serviced = self.dispatch(
            plugin::OpCode::GetMidiProgramName,
            channel,
            0,
            name.as_mut_ptr() as *mut c_void,
            0.0,
        );

        // "number of used programs, 0 = unsupported". A negative count is a
        // malformed answer, not a small one — `as usize` on it would be
        // catastrophic upstream, so reject it here.
        if serviced <= 0 {
            return None;
        }

        // SAFETY: zeroed above, hence initialised either way.
        Some((unsafe { name.assume_init() }, serviced as i32))
    }

    /// Query `effGetCurrentMidiProgram` — which program `channel` is on now.
    ///
    /// `None` on a negative answer, which is a refusal: all three measured
    /// plugins return `-1` here. Reading the struct on a `-1` return would
    /// report program 0 with an empty name as though it were real.
    ///
    /// **A `0` return is ambiguous and this method cannot resolve it.** The
    /// value is a program *index*, so `0` is both a valid answer and what an
    /// unimplemented opcode returns after falling through the dispatcher. A
    /// caller must disambiguate with an opcode whose zero is unambiguous —
    /// [`Self::midi_program_name`] reports a serviced count. The host wrapper in
    /// `tutti-vst2-host` does exactly that; see its `current_midi_program`.
    pub fn current_midi_program(
        &self,
        _main: &MainThread,
        channel: i32,
    ) -> Option<(api::MidiProgramName, i32)> {
        // SAFETY: as above — `#[repr(C)]` POD, all-zero is valid.
        let mut name: MaybeUninit<api::MidiProgramName> = MaybeUninit::zeroed();

        let current = self.dispatch(
            plugin::OpCode::GetCurrentMidiProgram,
            channel,
            0,
            name.as_mut_ptr() as *mut c_void,
            0.0,
        );

        if current < 0 {
            return None;
        }

        // SAFETY: zeroed above.
        Some((unsafe { name.assume_init() }, current as i32))
    }

    /// Query `effGetMidiProgramCategory` for `category_index` on `channel`.
    ///
    /// Returns the struct plus the plugin's count of used categories; `None`
    /// when unsupported or when the count is not positive.
    pub fn midi_program_category(
        &self,
        _main: &MainThread,
        channel: i32,
        category_index: i32,
    ) -> Option<(api::MidiProgramCategory, i32)> {
        // SAFETY: as above — `#[repr(C)]` POD, all-zero is valid.
        let mut cat: MaybeUninit<api::MidiProgramCategory> = MaybeUninit::zeroed();

        // SAFETY: zeroed and therefore initialised; offset from the same
        // allocation.
        unsafe {
            ptr::addr_of_mut!((*cat.as_mut_ptr()).this_category_index).write(category_index);
        }

        let serviced = self.dispatch(
            plugin::OpCode::GetMidiProgramCategory,
            channel,
            0,
            cat.as_mut_ptr() as *mut c_void,
            0.0,
        );

        if serviced <= 0 {
            return None;
        }

        // SAFETY: zeroed above.
        Some((unsafe { cat.assume_init() }, serviced as i32))
    }

    /// Query `effHasMidiProgramsChanged` — whether the program or key names on
    /// `channel` have changed since the host last read them.
    ///
    /// `true` only on an exact `1`. This is a cache-invalidation signal, so a
    /// garbage non-zero answer must not be read as "changed" forever.
    pub fn midi_programs_changed(&self, _main: &MainThread, channel: i32) -> bool {
        self.dispatch(
            plugin::OpCode::HasMidiProgramsChanged,
            channel,
            0,
            ptr::null_mut(),
            0.0,
        ) == 1
    }

    /// Query `effGetMidiKeyName` — the plugin's name for one MIDI note.
    ///
    /// Drum kits use this to label pads ("Kick", "Snare") instead of note
    /// numbers. `None` when unsupported.
    ///
    /// Both `program_index` and `key_number` are written into the struct before
    /// dispatch: they are the question. VST 2.4 specifies "1 = supported,
    /// 0 = not", so this compares against 1.
    pub fn midi_key_name(
        &self,
        _main: &MainThread,
        channel: i32,
        program_index: i32,
        key_number: i32,
    ) -> Option<api::MidiKeyName> {
        // SAFETY: as above — `#[repr(C)]` POD, all-zero is valid.
        let mut key: MaybeUninit<api::MidiKeyName> = MaybeUninit::zeroed();

        // SAFETY: zeroed and therefore initialised; offsets from the same
        // allocation.
        unsafe {
            ptr::addr_of_mut!((*key.as_mut_ptr()).this_program_index).write(program_index);
            ptr::addr_of_mut!((*key.as_mut_ptr()).this_key_number).write(key_number);
        }

        let supported = self.dispatch(
            plugin::OpCode::GetMidiKeyName,
            channel,
            0,
            key.as_mut_ptr() as *mut c_void,
            0.0,
        );

        if supported != 1 {
            return None;
        }

        // SAFETY: zeroed above.
        Some(unsafe { key.assume_init() })
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

/// `string` up to its first NUL, then cut to at most `max_len` bytes at a char boundary.
fn truncated_c_str(string: &str, max_len: usize) -> &str {
    let string = string.split('\0').next().unwrap_or("");
    let mut end = string.len().min(max_len);
    while !string.is_char_boundary(end) {
        end -= 1;
    }
    &string[..end]
}

/// The least room a string the plugin writes gets, whatever the spec's length is.
const MIN_STRING_BUFFER: usize = 256;

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
        // SAFETY: `get_effect` is the plugin's non-null `AEffect`, live until `effClose` on drop
        // of the core, so reading the `dispatcher` field is in bounds and initialised.
        let Some(dispatcher) = (unsafe { (*self.get_effect()).dispatcher }) else {
            // `PluginLoader::instance` refuses a null dispatcher, so only a plugin that
            // clears the slot later gets here; the opcode is unanswered.
            error!("plugin cleared its dispatcher; opcode {opcode:?} unanswered");
            return 0;
        };
        // SAFETY: `dispatcher` came out of the `AEffect` this instance owns, so
        // the effect pointer it is handed is the one it belongs to and is live
        // for the call. `ptr`/`value` are the caller's to get right — that is
        // the obligation `DispatcherProc` being `unsafe` now states out loud.
        unsafe { dispatcher(self.get_effect(), opcode.into(), index, value, ptr, opt) }
    }

    /// Send a lone opcode with no parameters.
    fn opcode(&self, opcode: plugin::OpCode) -> isize {
        self.dispatch(opcode, 0, 0, ptr::null_mut(), 0.0)
    }

    /// Like `dispatch`, except takes a `&str` to send via `ptr`, cut at its first NUL
    /// (a C string cannot hold one) and at `max_len` bytes, a char boundary at or below
    /// it. `max_len` is the room the opcode's buffer has without its NUL: the plugin
    /// `strcpy`s into a fixed buffer for some opcodes (`kVstMaxProgNameLen` for a
    /// program name), and a longer string would run past it.
    fn write_string(
        &self,
        opcode: plugin::OpCode,
        index: i32,
        value: isize,
        string: &str,
        max_len: usize,
        opt: f32,
    ) -> isize {
        let string = CString::new(truncated_c_str(string, max_len))
            .expect("`truncated_c_str` holds no interior NUL");
        self.dispatch(opcode, index, value, string.as_ptr() as *mut c_void, opt)
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
        // The spec's `max` is the size a plugin should write within; real plugins write
        // more, and hosts commonly offer 256, so the buffer is never smaller than that.
        let mut buf = vec![0; max.max(MIN_STRING_BUFFER)];
        self.dispatch(opcode, index, value, buf.as_mut_ptr() as *mut c_void, opt);
        String::from_utf8_lossy(&buf)
            .chars()
            .take_while(|c| *c != '\0')
            .collect()
    }
}

impl Dispatch for PluginInstance {
    fn get_effect(&self) -> *mut AEffect {
        self.core.get_effect()
    }
}

impl Dispatch for Core {
    fn get_effect(&self) -> *mut AEffect {
        // SAFETY: the cell is written once, in `PluginInstance::new`, before it is shared, and
        // never afterwards, so this read cannot race a write.
        unsafe { *self.effect.get() }
    }
}

impl Core {
    /// Send an `effSetChunk` blob (`index == 1` for a preset, `0` for the bank).
    ///
    /// The plugin gets a copy: the opcode's pointer is `void*`, and a plugin that
    /// writes through it (some do while parsing) would write into the caller's memory
    /// if it were handed `data` itself.
    fn set_chunk(&self, index: i32, data: &[u8]) -> bool {
        let mut copy = data.to_vec();
        self.dispatch(
            plugin::OpCode::SetData,
            index,
            copy.len() as isize,
            copy.as_mut_ptr() as *mut c_void,
            0.0,
        ) > 0
    }

    /// Read an `effGetChunk` blob (`index == 1` for the current preset,
    /// `index == 0` for the whole bank).
    ///
    /// `Ok(vec)` — possibly empty, meaning the plugin genuinely has nothing
    /// saved. `Err` — the plugin *attempted* the save and failed; see
    /// [`ChunkError`] for why the two must not be conflated.
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
    fn get_chunk(&self, index: i32) -> Result<Vec<u8>, ChunkError> {
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
/// Split out from [`Core::get_chunk`] so the validation is
/// testable without a loaded plugin.
///
/// `len == 0` is `Ok(empty)` — a normal state for an untouched chunk-capable
/// plugin. The two failure shapes get an `Err` rather than the empty `Vec` they
/// used to share with it, so a caller cannot read a failed save as "nothing
/// saved".
///
/// # Safety
/// If `len > 0` and `ptr` is non-null, `ptr` must be valid for reads of `len`
/// bytes.
unsafe fn copy_chunk(ptr: *mut u8, len: isize) -> Result<Vec<u8>, ChunkError> {
    if len < 0 {
        warn!("plugin returned a negative effGetChunk length ({len})");
        return Err(ChunkError::Failed(len));
    }
    if len == 0 {
        // Genuinely nothing saved — not an error.
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        warn!("plugin reported {len} bytes of chunk data but left the pointer null");
        return Err(ChunkError::NullBuffer(len));
    }
    // SAFETY: `len > 0` and `ptr` is non-null here (both checked above), and the caller's
    // contract (this fn's `# Safety`) is that `ptr` is readable for `len` bytes; `u8` has no
    // alignment or validity requirement.
    Ok(unsafe { slice::from_raw_parts(ptr, len as usize) }.to_vec())
}

/// What a block needs of a plugin: its `AEffect` and the flags and pin counts
/// the render entries check. `Copy`, and carrying no ownership: whoever holds
/// one keeps the plugin's core alive (`RenderHandle`, `PluginInstance`).
#[derive(Clone, Copy)]
struct RenderEntry {
    effect: *mut AEffect,
    inputs: usize,
    outputs: usize,
    can_replacing: bool,
    can_double_replacing: bool,
}

impl RenderEntry {
    fn dispatch(
        &self,
        opcode: plugin::OpCode,
        index: i32,
        value: isize,
        ptr: *mut c_void,
        opt: f32,
    ) -> isize {
        let _rendering = Rendering::enter(self.effect);
        // SAFETY: `effect` is the plugin's `AEffect`, live for as long as the
        // holder keeps the core (see the type's docs).
        let Some(dispatcher) = (unsafe { (*self.effect).dispatcher }) else {
            error!("plugin cleared its dispatcher; opcode {opcode:?} unanswered");
            return 0;
        };
        // SAFETY: as for `Dispatch::dispatch`: the effect is the dispatcher's
        // own, and `ptr` and `value` are the caller's to get right.
        unsafe { dispatcher(self.effect, opcode.into(), index, value, ptr, opt) }
    }

    /// Whether the plugin's pin counts, read from its `AEffect` now, still fit the
    /// buffer.
    ///
    /// A plugin may raise `numInputs` or `numOutputs` after activation
    /// (`audioMasterIOChanged`) without the host having re-activated it; its render
    /// entries then index the channel tables past the end of the buffer the host sized
    /// for the old counts. `inputs` and `outputs` here are those old counts, and a
    /// plugin that asks for more than the buffer holds is not called.
    fn pins_fit(&self, inputs: usize, outputs: usize) -> bool {
        // SAFETY: `effect` is live while the core is (see the type's docs); the counts are
        // plain `i32` fields, not calls into the plugin.
        let (live_inputs, live_outputs) =
            unsafe { ((*self.effect).numInputs, (*self.effect).numOutputs) };
        let fits = |live: i32, held: usize| usize::try_from(live).unwrap_or(0) <= held;
        fits(live_inputs, inputs) && fits(live_outputs, outputs)
    }

    /// Render one block.
    ///
    /// A plugin whose pin counts have grown past the buffer is not called and the
    /// outputs are silent (see [`pins_fit`](Self::pins_fit)); that is the `false`
    /// answer, and every other path answers `true`.
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
    ///
    /// The presence check is `Option`, not `(p as *const u8).is_null()`. That
    /// idiom silently did nothing: a bare `extern "C" fn` is non-nullable, so the
    /// compiler folded the comparison to `false` under `-O` and the guard shipped
    /// only in debug builds.
    fn process(&self, name: &str, buffer: &mut AudioBuffer<f32>) -> bool {
        let _rendering = Rendering::enter(self.effect);
        if buffer.input_count() < self.inputs {
            panic!("Too few inputs in AudioBuffer");
        }
        if buffer.output_count() < self.outputs {
            panic!("Too few outputs in AudioBuffer");
        }
        if !self.pins_fit(buffer.input_count(), buffer.output_count()) {
            // No log here: this runs every block on the audio thread. The caller counts
            // it (`Core::pin_refusals`) and the control side reports.
            zero_outputs(buffer);
            return false;
        }
        let effect = self.effect;
        let samples = buffer.samples() as i32;
        // SAFETY: `effect` is live for the holder's lifetime (type docs). The slot is read as
        // an `Option` fn pointer and only called when `Some`. The buffer has at least the
        // plugin's declared pin counts (checked above), each channel pointer addressing
        // `samples` frames (`AudioBuffer` invariant), and the plugin's own ABI is trusted.
        unsafe {
            if let Some(replacing) = (*effect).processReplacing.filter(|_| self.can_replacing) {
                replacing(
                    effect,
                    buffer.raw_inputs().as_ptr(),
                    buffer.raw_outputs().as_mut_ptr(),
                    samples,
                );
                return true;
            }

            let Some(accumulating) = (*effect)._process else {
                error!(
                    "plugin '{name}' installed neither processReplacing nor process; \
                     rendering silence"
                );
                zero_outputs(buffer);
                return true;
            };

            warn!(
                "plugin '{name}' does not support processReplacing; \
                 falling back to the deprecated accumulating process"
            );
            // Accumulating mode *adds* to the outputs, so they must start at 0.
            zero_outputs(buffer);
            accumulating(
                effect,
                buffer.raw_inputs().as_ptr(),
                buffer.raw_outputs().as_mut_ptr(),
                samples,
            );
        }
        true
    }

    /// Render one block in `f64`.
    ///
    /// There is no accumulating counterpart for double precision in VST2.4 —
    /// `processDoubleReplacing` is the only `f64` entry point — so an absent
    /// `effFlagsCanDoubleReplacing` or a null pointer leaves the caller to
    /// re-render through the `f32` path. Silence is returned rather than
    /// jumping to a null pointer; callers should consult
    /// `get_info().f64_precision` before choosing this path.
    fn process_f64(&self, name: &str, buffer: &mut AudioBuffer<f64>) -> bool {
        let _rendering = Rendering::enter(self.effect);
        if buffer.input_count() < self.inputs {
            panic!("Too few inputs in AudioBuffer");
        }
        if buffer.output_count() < self.outputs {
            panic!("Too few outputs in AudioBuffer");
        }
        if !self.pins_fit(buffer.input_count(), buffer.output_count()) {
            // No log here: this runs every block on the audio thread. The caller counts
            // it (`Core::pin_refusals`) and the control side reports.
            zero_outputs(buffer);
            return false;
        }
        let effect = self.effect;
        // SAFETY: as for `process`: live `effect`, the `Option` slot is called only when `Some`,
        // and the buffer has the declared pin counts with `samples` frames per channel.
        unsafe {
            let replacing = (*effect)
                .processReplacingF64
                .filter(|_| self.can_double_replacing);
            let Some(replacing) = replacing else {
                error!("plugin '{name}' does not support f64 processing; rendering silence");
                zero_outputs(buffer);
                return true;
            };
            replacing(
                effect,
                buffer.raw_inputs().as_ptr(),
                buffer.raw_outputs().as_mut_ptr(),
                buffer.samples() as i32,
            );
        }
        true
    }

    fn process_events(&self, events: &api::Events) {
        self.dispatch(
            plugin::OpCode::ProcessEvents,
            0,
            0,
            events as *const _ as *mut _,
            0.0,
        );
    }
}

/// The audio thread's handle on a loaded plugin, held apart from its [`PluginInstance`]: a
/// host that renders on one thread while another runs the plugin's dispatcher keeps the
/// instance on the control side and this on the audio side, so neither holds a Rust
/// reference the other's calls alias.
///
/// It offers the render entries only: `processReplacing`, `processReplacingF64`,
/// `effProcessEvents`, `effStartProcess`, `effStopProcess` and `setParameter`. None of the
/// main thread's opcodes is a method here, so a Processor cannot make one by construction.
///
/// It shares the instance's core and **does not own the plugin**. Dropping it, even as the
/// last holder, does nothing: `effClose` is the [`PluginInstance`]'s drop, on its thread.
/// Every entry takes the core's gate first (`try_lock`: it never waits on the audio thread)
/// and is refused, without touching the `AEffect` the plugin freed, once the instance has
/// closed the plugin: `process` renders silence, the rest do nothing. An entry that finds
/// the gate held (the instance's close is waiting on it, or running) is refused the same
/// way.
///
/// ```compile_fail,E0599
/// # use tutti_vst2_ffi::host::RenderHandle;
/// // A main-thread opcode is not an audio-thread entry.
/// fn go(render: &mut RenderHandle) { render.resume(); }
/// ```
///
/// Mutation (run): add a `resume` method to the handle: the doctest compiles and fails.
pub struct RenderHandle {
    entry: RenderEntry,
    name: Arc<str>,
    /// What `entry.effect` points into: the gate says whether it is still the plugin's.
    core: Arc<Core>,
}

impl fmt::Debug for RenderHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RenderHandle")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

// SAFETY: the handle is a pointer to the plugin's `AEffect`, plain flags and an `Arc` of
// the core, which is `Send + Sync`. VST 2.4 lets `process`, `processReplacing`,
// `effProcessEvents`, `effStartProcess` and `effStopProcess` run on the audio thread, so
// moving the handle to one is what the format expects; the holder calls it from one
// thread at a time (every method takes `&mut self`), and the gate keeps every call off a
// closed plugin.
unsafe impl Send for RenderHandle {}

impl Drop for RenderHandle {
    /// If this is the last holder of the core (the instance closed and went first), the core
    /// frees the host box and may free the host state it keeps alive. That must not happen on
    /// the audio thread, which is where a Processor could be dropped. The graph keeps it off
    /// that thread (it retires a plugin node's unit, and so its `Processor`, to the control
    /// side: see `tutti_plugin_types::defer_to_main`, "Never from an audio thread", and
    /// `tutti-plugin/tests/vst2_in_process_retire.rs`), so this branch runs on a control or
    /// pool thread. **It must not run on an audio thread:** it takes a lock and allocates.
    /// Off the instance's thread, the last holder's share goes to the main thread to be freed
    /// there.
    fn drop(&mut self) {
        if Arc::strong_count(&self.core) == 1
            && std::thread::current().id() != self.core.main_thread
        {
            let core = Arc::clone(&self.core);
            tutti_plugin_types::defer_to_main(move |_main| drop(core));
        }
    }
}

/// Runs `f` on the entry with the gate held and open, or answers `refused` when the plugin is
/// closed or dead, or the instance's close (or an exclusive call) holds the gate.
///
/// **A panic under the gate kills the core.** The mutex is then poisoned: the plugin may be in
/// any state (the panic came out of an entry mid-call), so the next entry marks the gate
/// closed and refuses, and every later one finds it so: the handle renders silence from then
/// on, and the plugin is closed by the instance's drop as ever. Free (not a method), so the
/// caller can keep borrowing the handle's other fields (the name) across the call.
fn gated<R>(
    core: &Core,
    entry: &RenderEntry,
    refused: R,
    f: impl FnOnce(&RenderEntry, &Core) -> R,
) -> R {
    let gate = match core.gate.try_lock() {
        Ok(gate) => gate,
        Err(TryLockError::Poisoned(poisoned)) => {
            *poisoned.into_inner() = true;
            return refused;
        }
        Err(TryLockError::WouldBlock) => return refused,
    };
    if *gate {
        return refused;
    }
    // The guard is held to the end of this function, so for the whole call; a panic out of `f`
    // unwinds through it and poisons the mutex.
    f(entry, core)
}

impl RenderHandle {
    /// Whether this handle and `instance` are on the same plugin.
    pub fn is_of(&self, instance: &PluginInstance) -> bool {
        Arc::ptr_eq(&self.core, &instance.core)
    }

    /// Render one block in `f32`: `processReplacing`, or the deprecated accumulating
    /// `process` for a plugin that lacks it, or silence when it has neither or the plugin
    /// is closed.
    ///
    /// # Panics
    ///
    /// If the buffer has fewer channels than the plugin had pins when the handle
    /// was made. A plugin that raised its pin counts since is not called: the
    /// outputs are silent.
    pub fn process(&mut self, buffer: &mut AudioBuffer<f32>) {
        let name: &str = &self.name;
        let rendered = gated(&self.core, &self.entry, None, |entry, _| {
            Some(entry.process(name, buffer))
        });
        match rendered {
            Some(true) => {}
            Some(false) => {
                self.core.pin_refusals.fetch_add(1, Ordering::Relaxed);
            }
            None => zero_outputs(buffer),
        }
    }

    /// Render one block in `f64`: `processReplacingF64`, or silence when the plugin lacks it
    /// or is closed.
    ///
    /// # Panics
    ///
    /// As [`process`](Self::process).
    pub fn process_f64(&mut self, buffer: &mut AudioBuffer<f64>) {
        let name: &str = &self.name;
        let rendered = gated(&self.core, &self.entry, None, |entry, _| {
            Some(entry.process_f64(name, buffer))
        });
        match rendered {
            Some(true) => {}
            Some(false) => {
                self.core.pin_refusals.fetch_add(1, Ordering::Relaxed);
            }
            None => zero_outputs(buffer),
        }
    }

    /// Delivers MIDI events, `effProcessEvents`.
    pub fn process_events(&mut self, events: &api::Events) {
        gated(&self.core, &self.entry, (), |entry, _| {
            entry.process_events(events)
        });
    }

    /// `effStartProcess`, if processing is not on. Only legal while the plugin is resumed.
    pub fn start_process(&mut self) {
        gated(&self.core, &self.entry, (), |entry, core| {
            if !core.processing.swap(true, Ordering::AcqRel) {
                entry.dispatch(plugin::OpCode::StartProcess, 0, 0, ptr::null_mut(), 0.0);
            }
        });
    }

    /// `effStopProcess`, if processing is on. Only legal while the plugin is resumed.
    pub fn stop_process(&mut self) {
        gated(&self.core, &self.entry, (), |entry, core| {
            if core.processing.swap(false, Ordering::AcqRel) {
                entry.dispatch(plugin::OpCode::StopProcess, 0, 0, ptr::null_mut(), 0.0);
            }
        });
    }

    /// Whether `effStartProcess` is the plugin's last word.
    pub fn is_processing(&self) -> bool {
        self.core.processing.load(Ordering::Acquire)
    }

    /// `setParameter`, which VST 2.4 allows from either thread. `false` when the plugin left
    /// the slot null, or is closed.
    pub fn set_parameter(&mut self, index: i32, value: f32) -> bool {
        gated(&self.core, &self.entry, false, |entry, _| {
            let effect = entry.effect;
            // SAFETY: the gate is held open, so the plugin is not closed and `effect` is its
            // live `AEffect`. The slot is read as an `Option` fn pointer and only called when
            // `Some`; `index` is the caller's to bound, as for the instance's
            // `set_parameter`.
            let Some(set) = (unsafe { (*effect).setParameter }) else {
                return false;
            };
            // SAFETY: the setter belongs to this effect, which is live (above).
            unsafe { set(effect, index, value) };
            true
        })
    }
}

// SAFETY: the instance is a pointer to the plugin's `AEffect` (through the core), an `Arc` of
// that core (which is `Send + Sync`) and a library handle. `Send` is what lets a host load on
// one thread and own the plugin on another. It permits moving only: the entries that VST2
// puts on the main thread take a `&MainThread`, which only that thread holds, and a drop off
// the thread the instance was made on leaks the plugin instead of closing it (see the type's
// docs). It is not `Sync` (`_not_sync`).
unsafe impl Send for PluginInstance {}

/// The entries of the main thread. See the type's docs for which take a token and why.
impl PluginInstance {
    /// The plugin's `AEffect` snapshot: name, vendor, category and the pin, parameter and
    /// program counts as they were read after `effOpen` (see
    /// [`refresh_effect_fields`](Self::refresh_effect_fields)).
    pub fn get_info(&self) -> Info {
        self.info.clone()
    }

    /// Dispatch `effOpen`, then re-read the `AEffect` fields the plugin may
    /// have finalised in it (pin and parameter counts, program count,
    /// `initialDelay`): a plugin, a shell plugin above all, settles on which
    /// effect it is during init, and [`get_info`](Self::get_info) and the
    /// process entry points' channel-count checks read this snapshot.
    pub fn init(&mut self, _main: &MainThread) {
        self.opcode(plugin::OpCode::Initialize);
        self.refresh_effect_fields();
    }

    /// Runs one of the **exclusive** calls: the ones a plugin cannot take while it is
    /// processing (`effSetSampleRate`, `effSetBlockSize`, `effSetProcessPrecision`,
    /// `effMainsChanged`). They hold the audio gate for the call, so they wait for an audio
    /// entry in flight and refuse (to silence) any that starts meanwhile: the plugin never
    /// sees one beside an audio entry, whoever calls it. The host crate also keeps them to
    /// the activation and the rejoin, where no audio handle is live, so the wait is never
    /// taken there; the gate is what makes the serialisation true for any other caller. The
    /// token-free dispatcher entries the in-process backend uses off the main thread (the
    /// parameter's name, label, display and properties, `effCanBeAutomated`,
    /// `effString2Parameter`, the off-main chunk read) take it too, for the same reason.
    fn exclusive<R>(&self, call: impl FnOnce() -> R) -> R {
        let _gate = self
            .core
            .gate
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        call()
    }

    /// `effSetSampleRate`; exclusive (see [`PluginInstance`]'s docs).
    pub fn set_sample_rate(&mut self, _main: &MainThread, rate: f32) {
        self.exclusive(|| {
            self.dispatch(plugin::OpCode::SetSampleRate, 0, 0, ptr::null_mut(), rate);
        });
    }

    /// `effSetBlockSize`; exclusive.
    pub fn set_block_size(&mut self, _main: &MainThread, size: i64) {
        self.exclusive(|| {
            self.dispatch(
                plugin::OpCode::SetBlockSize,
                0,
                size as isize,
                ptr::null_mut(),
                0.0,
            );
        });
    }

    /// `effMainsChanged(1)`; exclusive.
    pub fn resume(&mut self, _main: &MainThread) {
        self.exclusive(|| {
            self.dispatch(plugin::OpCode::StateChanged, 0, 1, ptr::null_mut(), 0.0);
        });
        self.resumed = true;
    }

    /// `effMainsChanged(0)`, if the plugin is resumed: a plugin already suspended would see a
    /// second one with no start before it. Exclusive.
    pub fn suspend(&mut self, _main: &MainThread) {
        if self.resumed {
            self.exclusive(|| {
                self.dispatch(plugin::OpCode::StateChanged, 0, 0, ptr::null_mut(), 0.0);
            });
            self.resumed = false;
        }
    }

    /// Whether the plugin is resumed (`effMainsChanged(1)` and no suspend since).
    pub fn is_resumed(&self) -> bool {
        self.resumed
    }

    /// Dispatch `effSetProcessPrecision`, carrying the width in `value`:
    /// `0` = 32-bit, `1` = 64-bit. Exclusive.
    pub fn set_precision(&mut self, _main: &MainThread, double: bool) {
        self.exclusive(|| {
            self.dispatch(
                plugin::OpCode::SetPrecision,
                0,
                double as isize,
                ptr::null_mut(),
                0.0,
            );
        });
    }

    /// `effCanDo`.
    pub fn can_do(&self, _main: &MainThread, can_do: plugin::CanDo) -> Supported {
        let s: String = can_do.into();
        // `Supported::from` is total: undocumented `effCanDo` returns (a string
        // length, an uninitialised stack slot) become `Supported::Custom`.
        // Panicking here would abort during *load*, since the host queries
        // canDo while probing every plugin.
        Supported::from(self.write_string(plugin::OpCode::CanDo, 0, 0, &s, usize::MAX, 0.0))
    }

    /// The name and arrangement of one input, `effGetInputProperties`.
    pub fn get_input_info(&self, _main: &MainThread, input: i32) -> ChannelInfo {
        ChannelInfo::from(self.channel_properties(plugin::OpCode::GetInputInfo, input))
    }

    /// The name and arrangement of one output, `effGetOutputProperties`.
    pub fn get_output_info(&self, _main: &MainThread, output: i32) -> ChannelInfo {
        ChannelInfo::from(self.channel_properties(plugin::OpCode::GetOutputInfo, output))
    }

    /// Whether the plugin publishes a GUI at all: `effFlagsHasEditor`.
    ///
    /// **Asks the plugin's flag, not the call count.** The flag is the one VST 2.4 defines
    /// for this query, and the one `vst::main` sets plugin-side when it has an editor.
    /// Deliberately *not* also requiring a successful `effEditGetRect`: the SDK lets a
    /// plugin compute its rect only once the editor is open, so requiring it would turn a
    /// plugin that has an editor into one that has not.
    pub fn has_editor(&self) -> bool {
        self.has_editor
    }

    /// The editor's size, `effEditGetRect`; `(0, 0)` when the plugin gives no rect.
    pub fn editor_size(&self, _main: &MainThread) -> (i32, i32) {
        // Assuming coordinate origins from top-left
        match self.editor_rect() {
            None => (0, 0),
            Some(rect) => (
                (rect.right - rect.left) as i32,
                (rect.bottom - rect.top) as i32,
            ),
        }
    }

    /// The editor's origin, `effEditGetRect`; `(0, 0)` when the plugin gives no rect.
    pub fn editor_position(&self, _main: &MainThread) -> (i32, i32) {
        // Assuming coordinate origins from top-left
        match self.editor_rect() {
            None => (0, 0),
            Some(rect) => (rect.left as i32, rect.top as i32),
        }
    }

    fn editor_rect(&self) -> Option<Rect> {
        let mut rect: *mut Rect = std::ptr::null_mut();
        let rect_ptr: *mut *mut Rect = &mut rect;

        let result = self.dispatch(
            plugin::OpCode::EditorGetRect,
            0,
            0,
            rect_ptr as *mut c_void,
            0.0,
        );

        if result == 0 || rect.is_null() {
            return None;
        }
        // SAFETY: `rect` is non-null (checked above) and `effEditGetRect` returned non-zero, so
        // per the VST2 contract the plugin set it to its own valid, aligned `ERect`; `Rect` is
        // `Copy`, so this copies it out. Validity is the plugin's word and is not checked, and
        // the plugin owns the memory (never freed here).
        Some(unsafe { *rect })
    }

    /// Dispatch `effEditOpen` into `parent`, reporting whether the plugin embedded its editor.
    ///
    /// Two corrections over the original:
    ///
    /// * **`> 0`, not `== 1`.** Shipping plugins return other positive values
    ///   (some the window handle), and `== 1` reported those successful opens as
    ///   refusals. `> 0` matches every other boolean opcode in this file.
    /// * **Refuse a second open.** VST 2.4 requires an `effEditClose` between
    ///   two `effEditOpen`s; opening twice leaks the first window in practice.
    ///   The already-open case answers `true`, since the editor *is* open, so a
    ///   caller does not read an idempotent open as a failure.
    pub fn open_editor(&mut self, _main: &MainThread, parent: WindowHandle) -> bool {
        if self.editor_open {
            warn!("effEditOpen called on an already-open editor; ignoring the second open");
            return true;
        }

        let result = self.dispatch(plugin::OpCode::EditorOpen, 0, 0, parent.as_ptr(), 0.0);

        let opened = result > 0;
        if opened {
            self.editor_open = true;
        }

        opened
    }

    /// Dispatch `effEditClose`, but only when an editor is actually open.
    ///
    /// The guard is load-bearing: hosts routinely have two closing paths (an
    /// explicit close and the drop), and a second `effEditClose` has no
    /// defined meaning in VST 2.4 — several real plugins double-free their
    /// window resources on it. Idempotence lives here, where `editor_open` lives.
    pub fn close_editor(&mut self, _main: &MainThread) {
        if !self.editor_open {
            return;
        }
        self.dispatch(plugin::OpCode::EditorClose, 0, 0, ptr::null_mut(), 0.0);
        self.editor_open = false;
    }

    /// Whether the editor is open.
    pub fn is_editor_open(&self) -> bool {
        self.editor_open
    }

    /// Dispatch `effEditIdle` while the editor is open.
    ///
    /// The host calls this on a timer (about 30 Hz) from its main thread;
    /// editors that animate or flush UI work do it here. Never before
    /// `effEditOpen` or after `effEditClose`: the opcode has no meaning for a
    /// window that does not exist.
    pub fn idle_editor(&mut self, _main: &MainThread) {
        if !self.editor_open {
            return;
        }
        self.dispatch(plugin::OpCode::EditorIdle, 0, 0, ptr::null_mut(), 0.0);
    }

    /// `effChangeProgram`.
    pub fn change_preset(&self, _main: &MainThread, preset: i32) {
        self.dispatch(
            plugin::OpCode::ChangePreset,
            0,
            preset as isize,
            ptr::null_mut(),
            0.0,
        );
    }

    /// `effGetProgram`.
    pub fn get_preset_num(&self, _main: &MainThread) -> i32 {
        self.opcode(plugin::OpCode::GetCurrentPresetNum) as i32
    }

    /// `effSetProgramName`.
    pub fn set_preset_name(&self, _main: &MainThread, name: String) {
        // `kVstMaxProgNameLen` counts the NUL.
        self.write_string(
            plugin::OpCode::SetCurrentPresetName,
            0,
            0,
            &name,
            MAX_PRESET_NAME_LEN - 1,
            0.0,
        );
    }

    /// `effGetProgramNameIndexed`.
    pub fn get_preset_name(&self, _main: &MainThread, preset: i32) -> String {
        self.read_string_param(
            plugin::OpCode::GetPresetName,
            preset,
            0,
            0.0,
            MAX_PRESET_NAME_LEN,
        )
    }

    /// The real `effGetChunk` answer for the current preset, with a failed save
    /// distinguishable from an empty one — what the [`ChunkError`] split exists for.
    ///
    /// # Errors
    ///
    /// [`ChunkError`] when the plugin attempted the save and failed.
    pub fn try_get_preset_data(&self, _main: &MainThread) -> Result<Vec<u8>, ChunkError> {
        self.core.get_chunk(1 /*preset*/)
    }

    /// Bank counterpart of [`try_get_preset_data`](Self::try_get_preset_data).
    ///
    /// # Errors
    ///
    /// As [`try_get_preset_data`](Self::try_get_preset_data).
    pub fn try_get_bank_data(&self, _main: &MainThread) -> Result<Vec<u8>, ChunkError> {
        self.core.get_chunk(0 /*bank*/)
    }

    /// `effGetChunk` for the current preset, **from any thread**: the one main-thread opcode
    /// with an entry that takes no token.
    ///
    /// It exists for `HostState::save_state` of the in-process backend, which the session
    /// calls off the main thread on purpose (the health snapshot is a job on the task pool,
    /// because a subprocess save is a blocking round trip with a ten-second timeout), behind
    /// the backend's lock. For that backend it is a chunk read beside the editor and the audio
    /// thread, which the SDK does not promise to be safe. Closing it is a main-thread route
    /// for the snapshot, which is a change to `tutti-session` and `bevy-tutti`, not to this
    /// crate; until then this is the named exception, and
    /// [`try_get_preset_data`](Self::try_get_preset_data) is the entry for a caller on the
    /// main thread.
    ///
    /// # Errors
    ///
    /// As [`try_get_preset_data`](Self::try_get_preset_data).
    pub fn chunk_off_main_thread(&self) -> Result<Vec<u8>, ChunkError> {
        self.exclusive(|| self.core.get_chunk(1 /*preset*/))
    }

    /// Hand a preset chunk to the plugin, reporting whether it **accepted** it.
    ///
    /// VST 2.4 has `effSetChunk` return `1` on success. `> 0`, not `== 1`, matching every
    /// other boolean opcode in this file: shipping plugins return other positive values for
    /// "yes", and reading those as refusal would lose presets that did load.
    pub fn load_preset_data(&self, _main: &MainThread, data: &[u8]) -> bool {
        self.core.set_chunk(1, data)
    }

    /// Bank counterpart of [`load_preset_data`](Self::load_preset_data); same
    /// accept/reject contract.
    pub fn load_bank_data(&self, _main: &MainThread, data: &[u8]) -> bool {
        self.core.set_chunk(0, data)
    }
}

/// The parameter surface hosts call from either thread; see the type's docs.
impl PluginInstance {
    /// `getParameter`. `None` when the plugin left `AEffect::getParameter` null, which VST 2.4
    /// permits for a plugin declaring `numParams == 0`.
    pub fn get_parameter(&self, index: i32) -> Option<f32> {
        let effect = self.core.get_effect();
        // SAFETY: `get_effect` returns the live `AEffect` this instance wraps: the instance is
        // the only closer, and it is borrowed here.
        let get = unsafe { (*effect).getParameter }?;
        // SAFETY: `get` came out of that same `AEffect`, so it is the effect's
        // own accessor being handed the effect it belongs to. `index` is the
        // caller's to bound — VST 2.4 gives the callee no way to report a bad
        // one, which is the obligation `GetParameterProc` being `unsafe` states.
        Some(unsafe { get(effect, index) })
    }

    /// `setParameter`. `false` when the plugin left `AEffect::setParameter` null — the value
    /// went nowhere.
    pub fn set_parameter(&self, index: i32, value: f32) -> bool {
        let effect = self.core.get_effect();
        // SAFETY: as above.
        let Some(set) = (unsafe { (*effect).setParameter }) else {
            return false;
        };
        // SAFETY: as in `get_parameter` — the setter belongs to this effect,
        // and `index` is the caller's obligation.
        unsafe { set(effect, index, value) };
        true
    }

    /// `effGetParamLabel`.
    pub fn get_parameter_label(&self, index: i32) -> String {
        self.exclusive(|| {
            self.read_string_param(
                plugin::OpCode::GetParameterLabel,
                index,
                0,
                0.0,
                MAX_PARAM_STR_LEN,
            )
        })
    }

    /// `effGetParamDisplay`.
    pub fn get_parameter_text(&self, index: i32) -> String {
        self.exclusive(|| {
            self.read_string_param(
                plugin::OpCode::GetParameterDisplay,
                index,
                0,
                0.0,
                MAX_PARAM_STR_LEN,
            )
        })
    }

    /// `effGetParamName`.
    pub fn get_parameter_name(&self, index: i32) -> String {
        self.exclusive(|| {
            self.read_string_param(
                plugin::OpCode::GetParameterName,
                index,
                0,
                0.0,
                MAX_PARAM_STR_LEN,
            )
        })
    }

    /// `effCanBeAutomated`.
    pub fn can_be_automated(&self, index: i32) -> bool {
        self.exclusive(|| {
            self.dispatch(
                plugin::OpCode::CanBeAutomated,
                index,
                0,
                ptr::null_mut(),
                0.0,
            ) > 0
        })
    }

    /// `effString2Parameter`: a setter, with no parse-only counterpart.
    pub fn string_to_parameter(&self, index: i32, text: String) -> bool {
        self.exclusive(|| {
            self.write_string(
                plugin::OpCode::StringToParameter,
                index,
                0,
                &text,
                usize::MAX,
                0.0,
            ) > 0
        })
    }
}

/// Used for constructing `AudioBuffer` instances on the host.
///
/// This struct contains all necessary allocations for an `AudioBuffer` apart
/// from the actual sample arrays. This way, the inner processing loop can
/// be allocation free even if `AudioBuffer` instances are repeatedly created.
///
/// ```rust
/// # use tutti_vst2_ffi::host::HostBuffer;
/// # use tutti_vst2_ffi::plugin::Plugin;
/// # fn test<P: Plugin>(plugin: &mut P) {
/// let mut host_buffer: HostBuffer<f32> = HostBuffer::new(2, 2);
/// let inputs = vec![vec![0.0; 1000]; 2];
/// let mut outputs = vec![vec![0.0; 1000]; 2];
/// let mut audio_buffer = host_buffer.bind(&inputs, &mut outputs);
/// plugin.process(&mut audio_buffer);
/// # }
/// ```
#[derive(Debug)]
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
    /// The buffer borrows the arrays for as long as it lives (`'a`), so the arrays cannot
    /// be dropped or reused while a plugin can still read or write them through it:
    ///
    /// ```compile_fail
    /// # use tutti_vst2_ffi::host::HostBuffer;
    /// // Mutation (run): give `bind` the old signature (`&'a mut self`, `&[I]`, `&mut [O]`,
    /// // `I: 'a`, `O: 'a`) and this compiles, so the doctest fails.
    /// let mut host_buffer: HostBuffer<f32> = HostBuffer::new(1, 1);
    /// let mut buffer = {
    ///     let inputs = vec![vec![0.0f32; 4]];
    ///     let mut outputs = vec![vec![0.0f32; 4]];
    ///     host_buffer.bind(&inputs, &mut outputs)
    /// };
    /// let _ = buffer.split();
    /// ```
    ///
    /// # Panics
    /// This function will panic if more inputs or outputs are supplied than the `HostBuffer`
    /// was created for, or if the sample arrays do not all have the same length.
    pub fn bind<'a, I, O>(
        &'a mut self,
        input_arrays: &'a [I],
        output_arrays: &'a mut [O],
    ) -> AudioBuffer<'a, T>
    where
        I: AsRef<[T]>,
        O: AsMut<[T]>,
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
        //
        // SAFETY: the first `input_arrays.len()` / `output_arrays.len()` slots of
        // `self.inputs`/`self.outputs` were just set (the counts fit, checked above), each
        // from a slice of `length` samples, and the output slices are distinct `&mut` so
        // they do not alias. The pointers address data reached through `input_arrays` /
        // `output_arrays`, which are borrowed for `'a` (the buffer's lifetime), as
        // `from_raw` requires; `self` is borrowed for `'a` too, so the pointer tables
        // outlive the buffer.
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

/// How many `PluginLoader::instance` calls are between the start of `main` and the
/// adoption (or refusal) of the effect it returned, on any thread.
static LOADS_IN_PROGRESS: AtomicUsize = AtomicUsize::new(0);

/// The `AEffect` addresses whose `reserved1` is a host box this crate wrote: from adoption
/// in `PluginLoader::instance` until the core's drop, after `effClose`.
static ADOPTED: Mutex<BTreeSet<usize>> = Mutex::new(BTreeSet::new());

thread_local! {
    /// The `AEffect` this thread is inside a render entry of (`RenderEntry::process`,
    /// `process_f64`, `dispatch`), null otherwise. A host callback the plugin makes from
    /// inside such a call names that effect, which the host adopted before it could
    /// render, so `callback_wrapper` needs no registry lookup (and no lock) for it.
    static RENDERING: Cell<*mut AEffect> = const { Cell::new(ptr::null_mut()) };
}

/// Sets [`RENDERING`] for its lifetime and restores the previous value (a render entry
/// can nest: `process_events` is dispatched from inside a block's work).
struct Rendering(*mut AEffect);

impl Rendering {
    fn enter(effect: *mut AEffect) -> Rendering {
        Rendering(RENDERING.replace(effect))
    }
}

impl Drop for Rendering {
    fn drop(&mut self) {
        RENDERING.set(self.0);
    }
}

/// Marks a load in progress for its lifetime.
struct Loading;

impl Loading {
    fn begin() -> Loading {
        // `SeqCst`: the increment happens before `main` runs, so every thread `main`
        // spawns observes it.
        LOADS_IN_PROGRESS.fetch_add(1, Ordering::SeqCst);
        Loading
    }
}

impl Drop for Loading {
    fn drop(&mut self) {
        LOADS_IN_PROGRESS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Registry lookups, to show in a test that the render path takes none.
#[cfg(test)]
static REGISTRY_LOCKS: AtomicUsize = AtomicUsize::new(0);

fn adopted() -> std::sync::MutexGuard<'static, BTreeSet<usize>> {
    #[cfg(test)]
    REGISTRY_LOCKS.fetch_add(1, Ordering::SeqCst);
    ADOPTED
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn adopt(effect: *mut AEffect) {
    adopted().insert(effect as usize);
}

fn unadopt(effect: *mut AEffect) {
    adopted().remove(&(effect as usize));
}

thread_local! {
    /// The `*const Arc<T>` of the loader running `VSTPluginMain` on this thread, null otherwise.
    ///
    /// A plugin calls the host callback during `main` on the loading thread, before the host
    /// can store its pointer in `AEffect::reserved1`. Thread-local, so concurrent loads on
    /// different threads each see their own host without a lock. Once `main` returns the
    /// instance's `reserved1` carries the host, and this is null again.
    ///
    /// No process-wide load lock is needed for the host pointer. A callback during `main` from
    /// a thread the plugin spawned finds null and answers `0`.
    static LOAD_HOST: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
}

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
    //
    // SAFETY: which host a call reaches is decided without trusting `effect` until it is
    // known to be ours.
    //
    // 1. `LOAD_HOST` first. While this thread is inside `main` it points at the loader's
    //    `Arc<T>`, which outlives `main` (see `call_main`), and is the host for every call
    //    made on this thread: the `effect` a plugin passes then is not yet the host's
    //    (`reserved1` is "reserved for host", and a plugin that does not zero its `AEffect`
    //    leaves garbage there), so it is not read.
    // 2. No load in progress anywhere (`LOADS_IN_PROGRESS == 0`): every live effect has been
    //    adopted, so a non-null `effect` is readable (VST2 ABI) and its `reserved1`, when
    //    non-zero, is the `Box<Arc<T>>` set by `PluginLoader::instance`, kept alive by the
    //    core until after `effClose`; this callback is monomorphised for the same `T`. This
    //    is the steady-state path and takes no lock.
    // 3. A load is in progress and this thread has no `LOAD_HOST`:
    //    a. the call comes from inside a render entry of `effect` itself (`RENDERING`, set
    //       by `RenderEntry` for the duration of the plugin call, per thread): the effect
    //       was adopted before it could be rendered, so it is ours by construction. No
    //       registry, no lock: an audio thread playing one plugin is not made to contend
    //       while another loads.
    //    b. otherwise (a thread the plugin spawned, during `main` or not; a UI thread):
    //       `reserved1` is dereferenced only if `effect` is in the registry of adopted
    //       effects, else the call gets the load-time default answer, `0`. The registry
    //       is a mutex, taken only by those threads and only in this window. The count covers `main` through adoption
    //    (`instance` holds `Loading` across both), so an effect cannot be both unadopted
    //    and called after the count has dropped, except by a plugin that calls the host
    //    after its load ended with an `AEffect` the host refused or never adopted; that is
    //    a plugin outside the ABI and stays unchecked.
    //
    // A plugin that overwrites `reserved1` or calls the host after `effClose` breaks (2)
    // too, which is unchecked.
    let result = panic::catch_unwind(AssertUnwindSafe(|| unsafe {
        let load_ptr = LOAD_HOST.get() as *const Arc<T>;
        let host: &T = if !load_ptr.is_null() {
            // Used only during the plugin initialization.
            &*load_ptr
        } else if effect.is_null()
            || (LOADS_IN_PROGRESS.load(Ordering::SeqCst) != 0
                && effect != RENDERING.get()
                && !adopted().contains(&(effect as usize)))
        {
            return 0;
        } else if (*effect).reserved1 != 0 {
            // The plugin has already been initialized.
            &*((*effect).reserved1 as *const Arc<T>)
        } else {
            return 0;
        };
        interfaces::host_dispatch(host, effect, opcode, index, value, ptr, opt)
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

    /// `PluginInstance::start_process` / `stop_process` must dispatch the
    /// opcodes the plugin-side dispatcher in `interfaces.rs` actually matches
    /// on, and must not be left at the `Plugin` trait's empty default bodies.
    ///
    /// Asserting the numeric values is the point: `OpCode` is `#[repr(i32)]` and
    /// positional, so inserting a variant above these shifts every opcode below
    /// it and silently re-points the dispatch. The SDK fixes
    /// `effStartProcess = 71` and `effStopProcess = 72`.
    #[test]
    fn start_and_stop_process_opcodes_match_the_sdk() {
        assert_eq!(plugin::OpCode::StartProcess as i32, 71);
        assert_eq!(plugin::OpCode::StopProcess as i32, 72);

        // And the plugin-side dispatcher decodes those same numbers back to
        // the variants it handles — the other half of the round trip.
        assert!(matches!(
            plugin::OpCode::try_from(71),
            Ok(plugin::OpCode::StartProcess)
        ));
        assert!(matches!(
            plugin::OpCode::try_from(72),
            Ok(plugin::OpCode::StopProcess)
        ));
    }

    /// `effSetProcessPrecision = 77`, pinned for the same reason as the pair
    /// above. It sits four variants below `effStopProcess`, so it is the one
    /// most exposed to a variant inserted anywhere in that range.
    #[test]
    fn set_precision_opcode_matches_the_sdk() {
        assert_eq!(plugin::OpCode::SetPrecision as i32, 77);
        assert!(matches!(
            plugin::OpCode::try_from(77),
            Ok(plugin::OpCode::SetPrecision)
        ));
    }

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

        // SAFETY: `ChannelProperties` is `repr(C)` of byte arrays, an `i32` and a `repr(i32)` enum whose discriminant 0 (`Mono`) exists, so the all-zero bit pattern is a valid value.
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

        // SAFETY: `ChannelProperties` is `repr(C)` of byte arrays, an `i32` and a `repr(i32)` enum whose discriminant 0 (`Mono`) exists, so the all-zero bit pattern is a valid value.
        let props: api::ChannelProperties = unsafe { MaybeUninit::zeroed().assume_init() };
        let _info = ChannelInfo::from(props);
    }

    /// `effGetChunk` used to be trusted blindly:
    /// `slice::from_raw_parts(ptr, len as usize)` with no null check and no
    /// sign check. This is the project-*save* path, so each of these is a real
    /// crash on a real user's save.
    #[test]
    fn get_chunk_rejects_unreadable_plugin_answers() {
        // `effFlagsProgramChunks` set but nothing to save yet: the plugin
        // returns 0 and never writes the out-pointer. `from_raw_parts(null, 0)`
        // is UB even at length zero — the pointer must always be non-null.
        //
        // `Ok(empty)`, not an error: the plugin answered, and the answer is
        // "nothing saved".
        // SAFETY: `copy_chunk` reads through `ptr` only when `len > 0` and `ptr` is non-null; here `len <= 0` so it returns before any read.
        assert_eq!(unsafe { copy_chunk(ptr::null_mut(), 0) }, Ok(Vec::new()));

        // An error return. `-1 as usize` was `usize::MAX`.
        assert_eq!(
            // SAFETY: `copy_chunk` reads through `ptr` only when `len > 0` and `ptr` is non-null; here `len <= 0` so it returns before any read.
            unsafe { copy_chunk(ptr::null_mut(), -1) },
            Err(ChunkError::Failed(-1))
        );

        // A plugin that sized the chunk but failed to hand back the buffer.
        assert_eq!(
            // SAFETY: `copy_chunk` returns `NullBuffer` on a null `ptr` with `len > 0` before any read.
            unsafe { copy_chunk(ptr::null_mut(), 4096) },
            Err(ChunkError::NullBuffer(4096))
        );

        // A non-null pointer with a negative length is still rejected — the
        // length is what would be cast, and it must never reach `as usize`.
        let mut data = [1u8, 2, 3, 4];
        assert_eq!(
            // SAFETY: `len` is negative, so `copy_chunk` returns before any read; `data` is a live 4-byte array regardless.
            unsafe { copy_chunk(data.as_mut_ptr(), -1) },
            Err(ChunkError::Failed(-1))
        );

        // The good case still copies.
        assert_eq!(
            // SAFETY: `data` is a live local 4-byte array, so `ptr` is valid for reads of `len` = 4 bytes.
            unsafe { copy_chunk(data.as_mut_ptr(), 4) },
            Ok(vec![1, 2, 3, 4])
        );
    }

    /// `effEditOpen` / `effEditClose` must each be dispatched at most once per
    /// open editor, and a truthy-but-not-1 open must count as success.
    ///
    /// Three defects, all observable by counting dispatches: `open` demanded
    /// `result == 1` (rejecting truthy non-1 answers some plugins give);
    /// neither `open` nor `close` checked `is_open`, so a repeat open leaked the
    /// first window and a host's two teardown paths double-closed.
    #[test]
    fn the_editor_opens_once_closes_once_and_accepts_a_truthy_open() {
        use crate::api::DispatcherProc;
        use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};

        static OPEN_ANSWER: AtomicIsize = AtomicIsize::new(1);
        static OPENS: AtomicUsize = AtomicUsize::new(0);
        static CLOSES: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn dispatch(
            _effect: *mut AEffect,
            opcode: i32,
            _index: i32,
            _value: isize,
            _ptr: *mut c_void,
            _opt: f32,
        ) -> isize {
            if opcode == plugin::OpCode::EditorOpen as i32 {
                OPENS.fetch_add(1, Ordering::SeqCst);
                return OPEN_ANSWER.load(Ordering::SeqCst);
            }
            if opcode == plugin::OpCode::EditorClose as i32 {
                CLOSES.fetch_add(1, Ordering::SeqCst);
            }
            0
        }

        // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
        let mut effect: AEffect = unsafe { std::mem::zeroed() };
        effect.magic = VST_MAGIC;
        effect.dispatcher = Some(dispatch as DispatcherProc);
        let (mut ed, main) = instance_over(&mut effect as *mut AEffect);

        let reset = || {
            OPENS.store(0, Ordering::SeqCst);
            CLOSES.store(0, Ordering::SeqCst);
        };

        // Closing an editor that was never opened must not reach the plugin.
        reset();
        ed.close_editor(&main);
        assert_eq!(
            CLOSES.load(Ordering::SeqCst),
            0,
            "effEditClose dispatched for an editor that was never open"
        );

        // A plugin answering 2 has opened successfully.
        reset();
        OPEN_ANSWER.store(2, Ordering::SeqCst);
        assert!(
            ed.open_editor(&main, no_window()),
            "a truthy non-1 effEditOpen return must read as success; `== 1` \
             reported an embedded editor as a refusal"
        );

        // A second open must not reach the plugin.
        ed.open_editor(&main, no_window());
        assert_eq!(
            OPENS.load(Ordering::SeqCst),
            1,
            "effEditOpen dispatched twice without an intervening close — \
             undefined per VST 2.4"
        );

        // Close twice: only the first reaches the plugin.
        ed.close_editor(&main);
        ed.close_editor(&main);
        assert_eq!(
            CLOSES.load(Ordering::SeqCst),
            1,
            "effEditClose dispatched twice — close_editor() plus Drop is \
             exactly the sequence a host runs"
        );

        // A refusal is still a refusal, so none of the above is satisfied by
        // always reporting success.
        reset();
        OPEN_ANSWER.store(0, Ordering::SeqCst);
        assert!(
            !ed.open_editor(&main, no_window()),
            "effEditOpen returned 0 (refused)"
        );
        ed.close_editor(&main);
        assert_eq!(
            CLOSES.load(Ordering::SeqCst),
            0,
            "an editor whose open was refused is not open, so closing it must \
             not reach the plugin"
        );
    }

    /// `PluginInstance::idle_editor` dispatches `effEditIdle` while the editor is open, and
    /// only then.
    ///
    /// An idle that dispatched nothing would freeze editors that animate in `effEditIdle`.
    ///
    /// Mutation (run): make `idle_editor` an empty body -> the open
    /// editor's idle dispatches nothing -> fails. Mutation (run): drop the
    /// `is_open` guard -> an idle before open or after close reaches the
    /// plugin -> fails.
    #[test]
    fn idle_dispatches_effeditidle_only_while_the_editor_is_open() {
        use crate::api::DispatcherProc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        static IDLES: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn dispatch(
            _effect: *mut AEffect,
            opcode: i32,
            _index: i32,
            _value: isize,
            _ptr: *mut c_void,
            _opt: f32,
        ) -> isize {
            if opcode == plugin::OpCode::EditorOpen as i32 {
                return 1;
            }
            if opcode == plugin::OpCode::EditorIdle as i32 {
                IDLES.fetch_add(1, Ordering::SeqCst);
            }
            0
        }

        // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
        let mut effect: AEffect = unsafe { std::mem::zeroed() };
        effect.dispatcher = Some(dispatch as DispatcherProc);
        effect.magic = VST_MAGIC;
        let (mut ed, main) = instance_over(&mut effect as *mut AEffect);

        IDLES.store(0, Ordering::SeqCst);
        ed.idle_editor(&main);
        assert_eq!(IDLES.load(Ordering::SeqCst), 0, "idle before open");

        assert!(ed.open_editor(&main, no_window()));
        ed.idle_editor(&main);
        ed.idle_editor(&main);
        assert_eq!(IDLES.load(Ordering::SeqCst), 2, "one effEditIdle per idle");

        ed.close_editor(&main);
        ed.idle_editor(&main);
        assert_eq!(IDLES.load(Ordering::SeqCst), 2, "idle after close");
    }

    /// `audioMasterCanDo` reaches `Host::can_do` with the plugin's string, and
    /// the three VST 2.4 answers come back as `1`, `0` and `-1`.
    ///
    /// Before the arm existed the opcode logged the query and fell through to
    /// `0`, so a plugin gating a feature on `canDo("sendVstEvents")` was told
    /// "don't know" for everything.
    ///
    /// Mutation (run): the arm back to a log and fall-through -> all three
    /// answers read `0` -> fails. Mutation (run): `Supported::No => 0` ->
    /// the refusal reads as "don't know" -> fails.
    #[test]
    fn host_can_do_reaches_the_host_and_maps_to_the_three_answers() {
        struct Answers;
        impl Host for Answers {
            fn can_do(&self, can_do: &str) -> Supported {
                match can_do {
                    "sendVstEvents" => Supported::Yes,
                    "openFileSelector" => Supported::No,
                    _ => Supported::Maybe,
                }
            }
        }

        let ask = |name: &str| {
            let name = CString::new(name).unwrap();
            interfaces::host_dispatch(
                &Answers,
                ptr::null_mut(),
                OpCode::CanDo as i32,
                0,
                0,
                name.as_ptr() as *mut c_void,
                0.0,
            )
        };
        assert_eq!(ask("sendVstEvents"), 1);
        assert_eq!(ask("openFileSelector"), -1);
        assert_eq!(ask("somethingElse"), 0);
    }

    /// The answer a plugin's `effSetChunk` gives must reach the caller.
    ///
    /// Non-vacuous because it drives the *real* dispatch path rather than the
    /// trait default: a synthetic `AEffect` returning a chosen value, read back
    /// through `PluginParametersInstance::load_preset_data`, which used to
    /// return `()` and drop that answer.
    ///
    /// VST 2.4 names `1` for success, but `> 0` is deliberate and asserted
    /// below: shipping plugins return other positive values.
    #[test]
    fn the_plugins_set_chunk_answer_reaches_the_caller() {
        use crate::api::DispatcherProc;
        use std::sync::atomic::{AtomicIsize, Ordering};

        /// What the next synthetic dispatch returns.
        static ANSWER: AtomicIsize = AtomicIsize::new(0);

        extern "C" fn dispatch(
            _effect: *mut AEffect,
            _opcode: i32,
            _index: i32,
            _value: isize,
            _ptr: *mut c_void,
            _opt: f32,
        ) -> isize {
            ANSWER.load(Ordering::SeqCst)
        }

        // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
        let mut effect: AEffect = unsafe { std::mem::zeroed() };
        effect.magic = VST_MAGIC;
        effect.dispatcher = Some(dispatch as DispatcherProc);
        let (params, main) = instance_over(&mut effect as *mut AEffect);
        let set = |v: isize| ANSWER.store(v, Ordering::SeqCst);

        // 0 — the plugin refused. This is the answer the discarded return hid.
        set(0);
        assert!(
            !params.load_preset_data(&main, b"state"),
            "effSetChunk returned 0 (refused) but the caller was told success — \
             this is the bug: a rejected preset restores as if it had loaded"
        );

        // 1 — the documented success value.
        set(1);
        assert!(params.load_preset_data(&main, b"state"));

        // Truthy-but-not-1 is success, matching every other boolean opcode
        // here. `== 1` would report a loaded preset as refused.
        set(2);
        assert!(
            params.load_preset_data(&main, b"state"),
            "a positive non-1 return must read as success, as it does for \
             can_be_automated / string_to_parameter"
        );

        // Negative is a refusal, never a truthy non-zero.
        set(-1);
        assert!(
            !params.load_preset_data(&main, b"state"),
            "a negative return must never be read as success"
        );

        // The bank variant shares the contract.
        set(0);
        assert!(!params.load_bank_data(&main, b"state"));
        set(1);
        assert!(params.load_bank_data(&main, b"state"));
    }

    /// A plugin's chunk **refusal** must also be representable through the
    /// trait, so plugin implementations can express one while the default stays
    /// "accepted" for plugins that simply store what they are given.
    #[test]
    fn a_chunk_refusal_is_representable() {
        use crate::plugin::PluginParameters;

        struct RefusingParams;
        impl PluginParameters for RefusingParams {
            fn load_preset_data(&self, _data: &[u8]) -> bool {
                false
            }
            fn try_get_preset_data(&self) -> Result<Vec<u8>, ChunkError> {
                Err(ChunkError::Failed(-1))
            }
        }

        assert!(
            !RefusingParams.load_preset_data(b"anything"),
            "a plugin must be able to reject a chunk"
        );
        assert!(
            RefusingParams.try_get_preset_data().is_err(),
            "a plugin must be able to report a failed chunk save"
        );

        // The default must still succeed, so neither assertion above is
        // satisfied by a blanket failure.
        struct AcceptingParams;
        impl PluginParameters for AcceptingParams {}
        assert!(
            AcceptingParams.load_preset_data(b"anything"),
            "the default must accept, so existing plugins are unaffected"
        );
        assert_eq!(AcceptingParams.try_get_preset_data(), Ok(Vec::new()));
    }

    /// A *failed* save and an *empty* save must not produce the same value.
    /// Both used to be `Vec::new()`, which let a host read a plugin's failure as
    /// "no chunk state" and silently write a parameter snapshot instead.
    #[test]
    fn failed_chunk_save_is_distinguishable_from_an_empty_one() {
        // SAFETY: `copy_chunk` reads through `ptr` only when `len > 0` and `ptr` is non-null; here `len <= 0` so it returns before any read.
        let empty = unsafe { copy_chunk(ptr::null_mut(), 0) };
        // SAFETY: `copy_chunk` reads through `ptr` only when `len > 0` and `ptr` is non-null; here `len <= 0` so it returns before any read.
        let failed = unsafe { copy_chunk(ptr::null_mut(), -1) };
        assert_ne!(
            empty, failed,
            "a refused effGetChunk must not look like an empty one"
        );
        assert!(empty.is_ok(), "nothing-saved is a normal answer");
        assert!(failed.is_err(), "an error return must surface as an error");
    }

    /// The fallback path clears the outputs before the accumulating
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

    /// The audio entry points must be *detectably* absent.
    ///
    /// They were typed as bare `extern "C" fn`, which the compiler knows to be
    /// non-null, so the `(p as *const u8).is_null()` guard folded to `false` under
    /// `-O`: the null check shipped in debug builds and a jump to address 0 shipped
    /// in release. `Option` makes the check survive optimization.
    ///
    /// This asserts the *type-level* property, which is what actually failed —
    /// a plugin's real null slot cannot be exercised without a hostile plugin
    /// binary, but a guard the optimizer can delete is the whole bug.
    #[test]
    fn a_null_entry_point_is_detectable_at_every_opt_level() {
        use crate::api::{DispatcherProc, ProcessProc, ProcessProcF64};

        // The bytes a plugin leaves in a slot it never fills in. Transmuted
        // per-field: every function slot is `Option` now, so an all-zero
        // `AEffect` would in fact be valid, but building one would assert far
        // less precisely than naming each slot that must read as absent.
        // SAFETY: `Option<fn>` has the size of a pointer and null is its `None` niche, so the zero word is a valid `None`.
        let absent: Option<ProcessProc> = unsafe { std::mem::transmute(0usize) };
        assert!(
            absent.is_none(),
            "a null processReplacing must read as absent; if this fails, the \
             guard in `process` is decorative and release builds jump to 0"
        );

        // SAFETY: `Option<fn>` has the size of a pointer and null is its `None` niche, so the zero word is a valid `None`.
        let absent_f64: Option<ProcessProcF64> = unsafe { std::mem::transmute(0usize) };
        assert!(absent_f64.is_none());

        // SAFETY: `Option<fn>` has the size of a pointer and null is its `None` niche, so the zero word is a valid `None`.
        let absent_dispatch: Option<DispatcherProc> = unsafe { std::mem::transmute(0usize) };
        assert!(
            absent_dispatch.is_none(),
            "dispatcher carries every host->plugin call, so a null one is hit on \
             the first opcode rather than only during audio"
        );

        // A real pointer still reads as present — the guard rejects null, not
        // everything. (Any `extern "C"` fn of the right shape will do; it is never called.)
        extern "C" fn present_process(
            _: *mut AEffect,
            _: *const *const f32,
            _: *mut *mut f32,
            _: i32,
        ) {
        }
        // SAFETY: `process_replacing` is an `extern "C"` fn of `ProcessProc`'s shape, so its address as a pointer-sized word is a valid `Some(ProcessProc)`; it is only compared, never called.
        let present: Option<ProcessProc> =
            unsafe { std::mem::transmute(present_process as *const () as usize) };
        assert!(present.is_some());

        // ABI unchanged: null pointer optimization keeps `Option<fn>` one pointer
        // wide, so the C struct the plugin writes into still has its original
        // layout. A regression here is an ABI break, not a lint.
        assert_eq!(
            std::mem::size_of::<Option<ProcessProc>>(),
            std::mem::size_of::<ProcessProc>(),
        );
        assert_eq!(
            std::mem::size_of::<Option<DispatcherProc>>(),
            std::mem::size_of::<*const u8>(),
        );

        // The parameter accessors, converted later than the four above. VST 2.4
        // lets a plugin with `numParams == 0` leave these null, so an ordinary
        // parameterless effect reaches the same jump-to-zero.
        use crate::api::{GetParameterProc, SetParameterProc};
        // SAFETY: `Option<fn>` has the size of a pointer and null is its `None` niche, so the zero word is a valid `None`.
        let absent_get: Option<GetParameterProc> = unsafe { std::mem::transmute(0usize) };
        assert!(absent_get.is_none());
        // SAFETY: `Option<fn>` has the size of a pointer and null is its `None` niche, so the zero word is a valid `None`.
        let absent_set: Option<SetParameterProc> = unsafe { std::mem::transmute(0usize) };
        assert!(absent_set.is_none());
        assert_eq!(
            std::mem::size_of::<Option<GetParameterProc>>(),
            std::mem::size_of::<GetParameterProc>(),
        );
    }

    /// The absent case must stay distinguishable from a real `0.0`.
    ///
    /// `PluginParameters::get_parameter` returns `Option<f32>` for exactly this
    /// reason. Upstream vst-rs returns a bare `f32`, mirroring the C API, which
    /// forces `0.0` to mean both "the plugin has no parameter access" and "the
    /// parameter is at zero" — and there is deliberately no infallible overload
    /// alongside it that would let a caller re-merge them by accident.
    ///
    /// Driven through the real accessor over a zeroed `AEffect` — exactly the
    /// struct a parameterless plugin presents, and a valid instance only
    /// *because* every function slot is `Option`. Before that conversion this
    /// test could not have been written, and the code it covers jumped to
    /// address zero.
    #[test]
    fn an_absent_parameter_accessor_is_not_a_real_zero() {
        // Every slot null: the plugin filled in nothing.
        // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
        let mut effect: AEffect = unsafe { std::mem::zeroed() };
        let (params, _main) = instance_over(&mut effect as *mut AEffect);

        assert_eq!(
            params.get_parameter(0),
            None,
            "a null getParameter must be reported as absent, not as a value"
        );
        assert!(
            !params.set_parameter(0, 0.75),
            "a null setParameter must report that the value went nowhere"
        );

        // `None` is not `Some(0.0)`: a plugin genuinely sitting at zero stays
        // distinguishable from one that cannot be asked. Collapsing the two is
        // what the signature exists to prevent, and there is no lossy overload
        // that would quietly do it for a caller.
        assert_ne!(
            params.get_parameter(0),
            Some(0.0),
            "absent and a real zero must stay distinguishable — if they merge, \
             callers have no way to recover which happened"
        );
    }

    #[test]
    fn host_buffer() {
        const LENGTH: usize = 1_000_000;
        let mut host_buffer: HostBuffer<f32> = HostBuffer::new(2, 2);
        let input_left = vec![1.0; LENGTH];
        let input_right = vec![1.0; LENGTH];
        let mut output_left = vec![0.0; LENGTH];
        let mut output_right = vec![0.0; LENGTH];
        // `bind` borrows the slices of arrays for the buffer's lifetime, so they outlive it.
        let inputs = [&input_left, &input_right];
        let mut outputs = [&mut output_left, &mut output_right];
        {
            let mut audio_buffer = host_buffer.bind(&inputs, &mut outputs);
            for (input, output) in audio_buffer.zip() {
                for (i, o) in input.iter().zip(output) {
                    *o = *i * 2.0;
                }
            }
        }
        assert_eq!(output_left, vec![2.0; LENGTH]);
        assert_eq!(output_right, vec![2.0; LENGTH]);
    }

    /// `Host` that answers `audioMasterCurrentId` with its own number.
    struct IdHost(i32);

    impl Host for IdHost {
        fn get_plugin_id(&self) -> i32 {
            self.0
        }
    }

    /// Rendezvous inside `main`, so both loads are mid-initialization at once.
    #[cfg(unix)]
    static IN_MAIN: std::sync::Barrier = std::sync::Barrier::new(2);

    #[cfg(unix)]
    thread_local! {
        /// What the host callback answered inside this thread's last `main`.
        static ANSWERED: Cell<isize> = const { Cell::new(-1) };
    }

    /// A `VSTPluginMain` that calls back during initialization, as plugins do, after both
    /// loading threads are inside it.
    #[cfg(unix)]
    extern "C" fn probing_main(callback: crate::api::HostCallbackProc) -> *mut AEffect {
        IN_MAIN.wait();
        // SAFETY: `callback` is `callback_wrapper`, which accepts a null effect while loading;
        // opcode 2 is `audioMasterCurrentId`.
        let answer = unsafe { callback(ptr::null_mut(), 2, 0, 0, ptr::null_mut(), 0.0) };
        ANSWERED.set(answer);
        ptr::null_mut()
    }

    #[cfg(unix)]
    fn loader(id: i32) -> (PluginLoader<IdHost>, Arc<IdHost>) {
        let host = Arc::new(IdHost(id));
        // The loader only runs `main`, so any library handle will do.
        let lib: Library = libloading::os::unix::Library::this().into();
        let loader = PluginLoader {
            main: probing_main,
            lib: Arc::new(lib),
            host: Arc::clone(&host),
        };
        (loader, host)
    }

    /// Two threads inside `main` at once each get their own host's answer, and the host `Arc`
    /// is released when the loader goes.
    ///
    /// Answers are collected and asserted after both joins, so a failure cannot strand the
    /// other thread at the barrier.
    ///
    /// Mutation: replace `LOAD_HOST` with a `static AtomicPtr` (store in `call_main`, load in
    /// `callback_wrapper`). Both threads store before the barrier opens, so in every iteration
    /// the thread that stored first reads the other's pointer, and the assertion fails.
    #[cfg(unix)]
    #[test]
    fn concurrent_loads_reach_their_own_host() {
        let threads: Vec<_> = [11, 22]
            .into_iter()
            .map(|id| {
                std::thread::spawn(move || {
                    (0..200)
                        .map(|_| {
                            let (mut loader, host) = loader(id);
                            assert!(loader.call_main().is_null());
                            let answer = ANSWERED.get();
                            drop(loader);
                            (answer, Arc::strong_count(&host))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for (t, id) in threads.into_iter().zip([11isize, 22]) {
            for (answer, count) in t.join().unwrap() {
                assert_eq!(answer, id, "callback reached another host");
                assert_eq!(count, 1, "loader leaked a host Arc");
            }
        }
    }

    /// A load started inside `main` on the same thread leaves the outer load's pointer in
    /// place when it returns.
    ///
    /// Mutation: make `Restore::drop` always `set(null)`; the outer pointer is lost and the
    /// assertion fails.
    #[cfg(unix)]
    #[test]
    fn nested_load_restores_the_outer_pointer() {
        thread_local! {
            static OUTER: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
            static AFTER_INNER: Cell<*mut c_void> = const { Cell::new(ptr::null_mut()) };
        }
        extern "C" fn inner_main(_: crate::api::HostCallbackProc) -> *mut AEffect {
            ptr::null_mut()
        }
        extern "C" fn outer_main(_: crate::api::HostCallbackProc) -> *mut AEffect {
            OUTER.set(LOAD_HOST.get());
            let host = Arc::new(IdHost(2));
            let lib: Library = libloading::os::unix::Library::this().into();
            let mut inner = PluginLoader {
                main: inner_main,
                lib: Arc::new(lib),
                host,
            };
            inner.call_main();
            AFTER_INNER.set(LOAD_HOST.get());
            ptr::null_mut()
        }

        let lib: Library = libloading::os::unix::Library::this().into();
        let mut outer = PluginLoader {
            main: outer_main,
            lib: Arc::new(lib),
            host: Arc::new(IdHost(1)),
        };
        outer.call_main();
        assert!(!OUTER.get().is_null());
        assert_eq!(AFTER_INNER.get(), OUTER.get());
        assert!(LOAD_HOST.get().is_null());
    }

    /// `main` leaves nothing behind on the loading thread: a callback after it returns, with
    /// no `reserved1` to go on, is answered `0`.
    ///
    /// Mutation: skip the `Restore` guard in `call_main`; `LOAD_HOST` keeps pointing at the
    /// dead loader and the null assertion fails.
    #[cfg(unix)]
    #[test]
    fn load_host_is_null_after_main() {
        // A one-party barrier would hang on the shared one, so run `main` on a pair.
        let other = std::thread::spawn(|| {
            let (mut loader, _host) = loader(2);
            loader.call_main();
        });
        let (mut loader, _host) = loader(1);
        loader.call_main();
        other.join().unwrap();
        assert!(LOAD_HOST.get().is_null());
        let answer = callback_wrapper::<IdHost>(ptr::null_mut(), 2, 0, 0, ptr::null_mut(), 0.0);
        assert_eq!(answer, 0);
    }

    /// The box `reserved1` points at is freed with its owner.
    #[test]
    fn host_ref_releases_its_arc() {
        let host = Arc::new(IdHost(3));
        let held = HostRef::new(Arc::clone(&host));
        assert_eq!(Arc::strong_count(&host), 2);
        drop(held);
        assert_eq!(Arc::strong_count(&host), 1);
    }

    /// A zeroed `AEffect` the test owns and leaks, as a plugin's would be: a valid
    /// magic and the given dispatcher.
    fn fake_effect(dispatcher: crate::api::DispatcherProc) -> *mut AEffect {
        // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
        let mut effect: Box<AEffect> = Box::new(unsafe { std::mem::zeroed() });
        effect.magic = VST_MAGIC;
        effect.dispatcher = Some(dispatcher);
        Box::into_raw(effect)
    }

    /// The library handle an instance over a fake `AEffect` keeps: this process's own.
    fn this_library() -> Arc<Library> {
        #[cfg(unix)]
        let lib: Library = libloading::os::unix::Library::this().into();
        #[cfg(windows)]
        let lib: Library = libloading::os::windows::Library::this()
            .expect("this process's own module")
            .into();
        Arc::new(lib)
    }

    /// An instance over a fake `AEffect`, and the proof of the thread it lives on.
    fn instance_over(effect: *mut AEffect) -> (PluginInstance, MainThread) {
        let main = MainThread::claim().expect("a test claims the main thread freely");
        let instance = PluginInstance::new(effect, this_library(), None, &main);
        (instance, main)
    }

    /// A window handle for an editor test: the fake plugin never reads it.
    fn no_window() -> WindowHandle {
        // SAFETY: the fake plugins of these tests never dereference the handle.
        unsafe { WindowHandle::from_ptr(ptr::null_mut()) }
    }

    /// **`effClose` is the instance's drop, on its thread, and no handle outlives it.**
    /// The plugin frees its `AEffect` in response to `effClose`, so a handle that reads it
    /// afterwards is the audit's SIGSEGV (S1). The render handle shares the core but does not
    /// own the plugin: it can neither close it (dropped last, it does nothing) nor reach it
    /// once closed (every entry takes the gate and finds it closed). The fake plugin poisons
    /// its slots on close, as freeing the `AEffect` does, so a call after the close is
    /// visible.
    ///
    /// Mutation (run): make `RenderHandle::gated` skip the `*gate` check: the render after
    /// the close calls the poisoned plugin (`RENDERS` moves) and its outputs are no longer
    /// silent. Mutation (run): send `Shutdown` from `Core`'s drop again instead of the
    /// instance's: dropping the instance while the handle lives no longer closes
    /// (`CLOSES == 0` after it).
    #[cfg(unix)]
    #[test]
    fn effclose_is_the_instances_and_no_handle_outlives_it() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CLOSES: AtomicUsize = AtomicUsize::new(0);
        static RENDERS: AtomicUsize = AtomicUsize::new(0);
        static OPCODES_AFTER_CLOSE: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn dispatch(
            effect: *mut AEffect,
            opcode: i32,
            _index: i32,
            _value: isize,
            _ptr: *mut c_void,
            _opt: f32,
        ) -> isize {
            // SAFETY: the test's leaked `AEffect`, valid for the program.
            if unsafe { (*effect).getParameter.is_none() } {
                OPCODES_AFTER_CLOSE.fetch_add(1, Ordering::SeqCst);
            }
            if opcode == plugin::OpCode::Shutdown as i32 {
                CLOSES.fetch_add(1, Ordering::SeqCst);
                // The plugin frees its `AEffect` here; poison what a late call sees.
                // SAFETY: as above.
                unsafe {
                    (*effect).getParameter = None;
                    (*effect).setParameter = None;
                    (*effect).processReplacing = None;
                }
            }
            0
        }
        extern "C" fn get_parameter(_: *mut AEffect, _: i32) -> f32 {
            0.5
        }
        extern "C" fn set_parameter(_: *mut AEffect, _: i32, _: f32) {}
        extern "C" fn render(
            _: *mut AEffect,
            _: *const *const f32,
            outputs: *mut *mut f32,
            n: i32,
        ) {
            RENDERS.fetch_add(1, Ordering::SeqCst);
            // SAFETY: the test's buffer has one output channel of `n` samples.
            unsafe { std::slice::from_raw_parts_mut(*outputs, n as usize) }.fill(1.0);
        }

        let effect = fake_effect(dispatch);
        // SAFETY: the test's leaked `AEffect`; nothing else touches it yet.
        unsafe {
            (*effect).getParameter = Some(get_parameter);
            (*effect).setParameter = Some(set_parameter);
            (*effect).processReplacing = Some(render);
            (*effect).numOutputs = 1;
            (*effect).flags = PluginFlags::CAN_REPLACING.bits();
        }
        let (instance, main) = instance_over(effect);
        let mut render = instance.render_handle(&main);
        let mut host: HostBuffer<f32> = HostBuffer::new(0, 1);
        let mut out = vec![vec![9.0f32; 4]];
        let silent: Vec<Vec<f32>> = Vec::new();

        // Open: the handle renders and sets, and the instance is the one that reads.
        render.process(&mut host.bind(&silent, &mut out));
        assert_eq!(RENDERS.load(Ordering::SeqCst), 1);
        assert_eq!(out[0][0], 1.0);
        assert!(render.set_parameter(0, 0.25));
        assert_eq!(instance.get_parameter(0), Some(0.5));
        assert_eq!(CLOSES.load(Ordering::SeqCst), 0);

        // The instance drops, the handle lives: that closes it, once.
        drop(instance);
        assert_eq!(
            CLOSES.load(Ordering::SeqCst),
            1,
            "effClose is the instance's drop, not the last handle's"
        );
        let after_close = OPCODES_AFTER_CLOSE.load(Ordering::SeqCst);

        // Closed: every entry is refused without calling the plugin.
        out[0].fill(9.0);
        render.process(&mut host.bind(&silent, &mut out));
        assert_eq!(
            RENDERS.load(Ordering::SeqCst),
            1,
            "a render reached a closed plugin"
        );
        assert!(
            out[0].iter().all(|s| *s == 0.0),
            "a closed plugin is silent"
        );
        assert!(!render.set_parameter(0, 0.75));
        render.start_process();
        render.stop_process();
        assert_eq!(
            OPCODES_AFTER_CLOSE.load(Ordering::SeqCst),
            after_close,
            "an entry reached the dispatcher of a closed plugin"
        );

        // The handle, last, closes nothing.
        drop(render);
        assert_eq!(
            CLOSES.load(Ordering::SeqCst),
            1,
            "effClose runs exactly once"
        );
    }

    /// **A drop off the instance's thread does not close there, and does not skip the close**:
    /// the sequence is handed to the main thread and runs at its next tick
    /// (`run_deferred`), once, and a live handle keeps working until then. A close on the
    /// wrong thread is what a plugin may not survive; a skipped one leaks the plugin and its
    /// licence seat. A drop has no token, so the thread is recorded and compared.
    ///
    /// Mutation (run): drop the thread comparison in `Drop for PluginInstance`: the drop on
    /// the spawned thread closes there (`CLOSES == 1` before the tick). Mutation (run): drop
    /// the `defer_to_main` call: the tick closes nothing (`CLOSES == 0`) and the last
    /// assertion fails. Mutation (run): drop the counter increment: `off_thread_drops` stays 0.
    #[cfg(unix)]
    #[test]
    fn a_drop_off_the_main_thread_is_closed_on_the_main_threads_next_tick() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CLOSES: AtomicUsize = AtomicUsize::new(0);
        static RENDERS: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn dispatch(
            _: *mut AEffect,
            opcode: i32,
            _: i32,
            _: isize,
            _: *mut c_void,
            _: f32,
        ) -> isize {
            if opcode == plugin::OpCode::Shutdown as i32 {
                CLOSES.fetch_add(1, Ordering::SeqCst);
            }
            0
        }
        extern "C" fn render(_: *mut AEffect, _: *const *const f32, _: *mut *mut f32, _: i32) {
            RENDERS.fetch_add(1, Ordering::SeqCst);
        }

        let effect = fake_effect(dispatch);
        // SAFETY: the test's leaked `AEffect`; nothing else touches it yet.
        unsafe {
            (*effect).processReplacing = Some(render);
            (*effect).flags = PluginFlags::CAN_REPLACING.bits();
        }
        let (instance, main) = instance_over(effect);
        let mut render = instance.render_handle(&main);
        assert_eq!(off_thread_drops(), 0);

        std::thread::spawn(move || drop(instance))
            .join()
            .expect("the drop does not panic");
        assert_eq!(
            CLOSES.load(Ordering::SeqCst),
            0,
            "effClose ran on a thread that is not the instance's"
        );
        assert_eq!(off_thread_drops(), 1, "the off-thread drop is counted");
        let mut host: HostBuffer<f32> = HostBuffer::new(0, 0);
        let none: Vec<Vec<f32>> = Vec::new();
        let mut none_out: Vec<Vec<f32>> = Vec::new();
        render.process(&mut host.bind(&none, &mut none_out));
        assert_eq!(
            RENDERS.load(Ordering::SeqCst),
            1,
            "the plugin stays valid for the render handle until the tick"
        );

        assert!(tutti_plugin_types::run_deferred(&main) >= 1);
        assert_eq!(
            CLOSES.load(Ordering::SeqCst),
            1,
            "the main thread's tick sent the close"
        );
        render.process(&mut host.bind(&none, &mut none_out));
        assert_eq!(
            RENDERS.load(Ordering::SeqCst),
            1,
            "and the handle then finds the plugin closed"
        );
        assert_eq!(tutti_plugin_types::run_deferred(&main), 0);
        assert_eq!(CLOSES.load(Ordering::SeqCst), 1, "once");
    }

    /// **A render handle that is the last holder frees the core on the main thread**, not on
    /// the thread it is dropped on (the audio thread, for a Processor): the core keeps the
    /// host box alive, and with it the host state. The instance closes first, on its thread;
    /// the handle then drops elsewhere.
    ///
    /// Mutation (run): drop the `defer_to_main` in `Drop for RenderHandle`: the host `Arc` is
    /// released on the spawned thread and the first count assertion fails.
    #[cfg(unix)]
    #[test]
    fn the_last_render_handle_dropped_off_the_main_thread_frees_nothing_there() {
        let host = Arc::new(IdHost(4));
        let effect = fake_effect(noop_dispatch);
        let main = MainThread::claim().unwrap();
        let instance = PluginInstance::new(
            effect,
            this_library(),
            Some(HostRef::new(Arc::clone(&host))),
            &main,
        );
        let render = instance.render_handle(&main);
        assert_eq!(Arc::strong_count(&host), 2, "the core holds the host");
        drop(instance);
        assert_eq!(
            Arc::strong_count(&host),
            2,
            "the render handle still holds the core"
        );

        std::thread::spawn(move || drop(render)).join().unwrap();
        assert_eq!(
            Arc::strong_count(&host),
            2,
            "the core was not freed on the dropping thread"
        );
        tutti_plugin_types::run_deferred(&main);
        assert_eq!(Arc::strong_count(&host), 1, "the main thread freed it");
    }

    #[cfg(unix)]
    extern "C" fn noop_dispatch(
        _: *mut AEffect,
        _: i32,
        _: i32,
        _: isize,
        _: *mut c_void,
        _: f32,
    ) -> isize {
        0
    }

    /// **A panic under the gate kills the handle**: the plugin may be in any state, so every
    /// later entry is refused (silence) and the plugin is never called again; the instance
    /// still closes it.
    ///
    /// Mutation (run): in `gated`, take the poisoned guard and carry on (the old
    /// `into_inner`): the next block reaches the plugin (`RENDERS` moves) and the silence
    /// assertion fails.
    #[cfg(unix)]
    #[test]
    fn a_panic_under_the_gate_kills_the_core() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static RENDERS: AtomicUsize = AtomicUsize::new(0);
        static CLOSES: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn dispatch(
            _: *mut AEffect,
            opcode: i32,
            _: i32,
            _: isize,
            _: *mut c_void,
            _: f32,
        ) -> isize {
            if opcode == plugin::OpCode::Shutdown as i32 {
                CLOSES.fetch_add(1, Ordering::SeqCst);
            }
            0
        }
        extern "C" fn render(
            _: *mut AEffect,
            _: *const *const f32,
            outputs: *mut *mut f32,
            n: i32,
        ) {
            RENDERS.fetch_add(1, Ordering::SeqCst);
            // SAFETY: the test's buffer has two output channels of `n` samples.
            unsafe { std::slice::from_raw_parts_mut(*outputs, n as usize) }.fill(1.0);
        }

        let effect = fake_effect(dispatch);
        // SAFETY: the test's leaked `AEffect`; nothing else touches it yet.
        unsafe {
            (*effect).numOutputs = 2;
            (*effect).processReplacing = Some(render);
            (*effect).flags = PluginFlags::CAN_REPLACING.bits();
        }
        let (instance, main) = instance_over(effect);
        let mut handle = instance.render_handle(&main);
        let none: Vec<Vec<f32>> = Vec::new();

        // A buffer with one output channel for a plugin with two: the entry panics under the gate.
        let mut host1: HostBuffer<f32> = HostBuffer::new(0, 1);
        let mut one = vec![vec![9.0f32; 4]];
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            handle.process(&mut host1.bind(&none, &mut one));
        }));
        assert!(panicked.is_err(), "the short buffer is refused by a panic");
        assert_eq!(RENDERS.load(Ordering::SeqCst), 0);

        // A good buffer afterwards: the core is dead, so it is silent and the plugin is not called.
        let mut host2: HostBuffer<f32> = HostBuffer::new(0, 2);
        let mut two = vec![vec![9.0f32; 4]; 2];
        handle.process(&mut host2.bind(&none, &mut two));
        assert_eq!(
            RENDERS.load(Ordering::SeqCst),
            0,
            "a plugin that a panic left in an unknown state was called again"
        );
        assert!(
            two.iter().flatten().all(|s| *s == 0.0),
            "and renders silence"
        );

        drop(instance);
        assert_eq!(
            CLOSES.load(Ordering::SeqCst),
            1,
            "the instance still closes it"
        );
    }

    /// **Dropping the instance on its own thread sends the close sequence in order**: the
    /// editor, `effStopProcess` if processing, `effMainsChanged(0)` if resumed, then
    /// `effClose`; a plugin that was not resumed or not processing sees neither.
    ///
    /// Mutation (run): swap the stop and suspend blocks in `PluginInstance::close`: the
    /// recorded order changes and the assertion fails. Mutation (run): drop the `resumed`
    /// guard: the second instance sees a suspend it never resumed from.
    #[cfg(unix)]
    #[test]
    fn the_close_sequence_is_in_order_and_guarded() {
        use std::sync::Mutex;
        static SEEN: Mutex<Vec<i32>> = Mutex::new(Vec::new());

        extern "C" fn dispatch(
            _: *mut AEffect,
            opcode: i32,
            _: i32,
            value: isize,
            _: *mut c_void,
            _: f32,
        ) -> isize {
            let mut seen = SEEN.lock().unwrap();
            if opcode == plugin::OpCode::StateChanged as i32 {
                seen.push(if value == 1 { 1000 } else { 1001 });
            } else {
                seen.push(opcode);
            }
            1
        }
        let take = || std::mem::take(&mut *SEEN.lock().unwrap());
        let wanted = |ops: &[plugin::OpCode]| ops.iter().map(|o| *o as i32).collect::<Vec<_>>();

        // Resumed and processing, editor open: all four, in order.
        let (mut instance, main) = instance_over(fake_effect(dispatch));
        let mut render = instance.render_handle(&main);
        instance.resume(&main);
        assert!(instance.open_editor(&main, no_window()));
        render.start_process();
        take();
        drop(instance);
        let mut expected = wanted(&[plugin::OpCode::EditorClose, plugin::OpCode::StopProcess]);
        expected.push(1001);
        expected.push(plugin::OpCode::Shutdown as i32);
        assert_eq!(take(), expected);
        drop(render);

        // Never resumed, never processing, no editor: just `effClose`.
        let (instance, _main) = instance_over(fake_effect(dispatch));
        take();
        drop(instance);
        assert_eq!(take(), wanted(&[plugin::OpCode::Shutdown]));
    }

    /// **The exclusive calls are serialised with the audio entries, both ways**: an
    /// `effSetSampleRate` waits for a render in flight, and a render that starts during one is
    /// refused (silence) without reaching the plugin. A plugin that cannot take the opcode
    /// beside `process` (doc 016 §2.10) therefore never sees them together, whoever calls.
    ///
    /// Mutation (run): drop the `lock()` in `PluginInstance::exclusive` (run the call
    /// unguarded): the rate is dispatched while the render is in flight (first part), and the
    /// render during the rate call reaches the plugin (second part); both assertions fail.
    #[cfg(unix)]
    #[test]
    fn the_exclusive_calls_are_serialised_with_the_audio_entries() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::Duration;
        static IN_PROCESS: AtomicBool = AtomicBool::new(false);
        static RELEASE_PROCESS: AtomicBool = AtomicBool::new(false);
        static RATE_DISPATCHED: AtomicBool = AtomicBool::new(false);
        static IN_RATE: AtomicBool = AtomicBool::new(false);
        static RELEASE_RATE: AtomicBool = AtomicBool::new(false);
        static PROCESSES: AtomicUsize = AtomicUsize::new(0);
        static FINISHED_PROCESSES: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn dispatch(
            _: *mut AEffect,
            opcode: i32,
            _: i32,
            _: isize,
            _: *mut c_void,
            _: f32,
        ) -> isize {
            if opcode == plugin::OpCode::SetSampleRate as i32 {
                // The rate must not be dispatched while a process is unfinished.
                assert_eq!(
                    PROCESSES.load(Ordering::SeqCst),
                    FINISHED_PROCESSES.load(Ordering::SeqCst),
                    "an exclusive call began inside an audio entry"
                );
                RATE_DISPATCHED.store(true, Ordering::SeqCst);
                IN_RATE.store(true, Ordering::SeqCst);
                while !RELEASE_RATE.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                IN_RATE.store(false, Ordering::SeqCst);
            }
            0
        }
        extern "C" fn render(
            _: *mut AEffect,
            _: *const *const f32,
            outputs: *mut *mut f32,
            n: i32,
        ) {
            PROCESSES.fetch_add(1, Ordering::SeqCst);
            IN_PROCESS.store(true, Ordering::SeqCst);
            while !RELEASE_PROCESS.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            // SAFETY: the test's buffer has one output channel of `n` samples.
            unsafe { std::slice::from_raw_parts_mut(*outputs, n as usize) }.fill(1.0);
            IN_PROCESS.store(false, Ordering::SeqCst);
            FINISHED_PROCESSES.fetch_add(1, Ordering::SeqCst);
        }

        let effect = fake_effect(dispatch);
        // SAFETY: the test's leaked `AEffect`; nothing else touches it yet.
        unsafe {
            (*effect).processReplacing = Some(render);
            (*effect).numOutputs = 1;
            (*effect).flags = PluginFlags::CAN_REPLACING.bits();
        }
        let (mut instance, main) = instance_over(effect);
        let mut handle = instance.render_handle(&main);
        let block = |handle: &mut RenderHandle| {
            let mut host: HostBuffer<f32> = HostBuffer::new(0, 1);
            let none: Vec<Vec<f32>> = Vec::new();
            let mut out = vec![vec![9.0f32; 4]];
            handle.process(&mut host.bind(&none, &mut out));
            out
        };

        // Part one: a render is in flight when the exclusive call is made. The call waits.
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let renderer = std::thread::spawn(move || {
            let out = block(&mut handle);
            handle_tx.send(handle).unwrap();
            out
        });
        while !IN_PROCESS.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let watcher = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            let early = RATE_DISPATCHED.load(Ordering::SeqCst);
            RELEASE_PROCESS.store(true, Ordering::SeqCst);
            early
        });
        RELEASE_RATE.store(true, Ordering::SeqCst);
        instance.set_sample_rate(&main, 48_000.0);
        assert!(
            !watcher.join().unwrap(),
            "the exclusive call ran while a render was in flight"
        );
        assert!(RATE_DISPATCHED.load(Ordering::SeqCst), "and it ran after");
        assert_eq!(renderer.join().unwrap()[0][0], 1.0);
        let mut handle = handle_rx.recv().unwrap();

        // Part two: a render starts while the exclusive call runs. It is refused.
        RATE_DISPATCHED.store(false, Ordering::SeqCst);
        RELEASE_RATE.store(false, Ordering::SeqCst);
        RELEASE_PROCESS.store(true, Ordering::SeqCst);
        let before = PROCESSES.load(Ordering::SeqCst);
        let during = std::thread::spawn(move || {
            while !IN_RATE.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            let out = block(&mut handle);
            RELEASE_RATE.store(true, Ordering::SeqCst);
            out
        });
        instance.set_sample_rate(&main, 44_100.0);
        let out = during.join().unwrap();
        assert_eq!(
            PROCESSES.load(Ordering::SeqCst),
            before,
            "a render reached the plugin during an exclusive call"
        );
        assert!(
            out[0].iter().all(|s| *s == 0.0),
            "and was refused to silence"
        );
    }

    /// **A token-free dispatcher entry is serialised with `process`, both ways**
    /// (`get_parameter_name` stands for the named exceptions: name, label, display,
    /// properties, `effCanBeAutomated`, `effString2Parameter`, the off-main chunk read). VST2
    /// lets only `getParameter` and `setParameter` run beside a render; these are dispatcher
    /// calls, so each waits for a render in flight and a render that starts meanwhile is
    /// refused.
    ///
    /// Mutation (run): drop the `exclusive` wrapper in `get_parameter_name`: the name opcode
    /// is dispatched while the render is in flight (first part; the plugin's own assertion
    /// aborts), and a render reaches the plugin during it (second part).
    #[cfg(unix)]
    #[test]
    fn a_parameter_name_and_process_are_serialised() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::Duration;
        static IN_PROCESS: AtomicBool = AtomicBool::new(false);
        static RELEASE_PROCESS: AtomicBool = AtomicBool::new(false);
        static NAME_DISPATCHED: AtomicBool = AtomicBool::new(false);
        static IN_NAME: AtomicBool = AtomicBool::new(false);
        static RELEASE_NAME: AtomicBool = AtomicBool::new(false);
        static PROCESSES: AtomicUsize = AtomicUsize::new(0);
        static FINISHED_PROCESSES: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn dispatch(
            _: *mut AEffect,
            opcode: i32,
            _: i32,
            _: isize,
            _: *mut c_void,
            _: f32,
        ) -> isize {
            if opcode == plugin::OpCode::GetParameterName as i32 {
                assert_eq!(
                    PROCESSES.load(Ordering::SeqCst),
                    FINISHED_PROCESSES.load(Ordering::SeqCst),
                    "a dispatcher call began inside an audio entry"
                );
                NAME_DISPATCHED.store(true, Ordering::SeqCst);
                IN_NAME.store(true, Ordering::SeqCst);
                while !RELEASE_NAME.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                IN_NAME.store(false, Ordering::SeqCst);
            }
            0
        }
        extern "C" fn render(
            _: *mut AEffect,
            _: *const *const f32,
            outputs: *mut *mut f32,
            n: i32,
        ) {
            PROCESSES.fetch_add(1, Ordering::SeqCst);
            IN_PROCESS.store(true, Ordering::SeqCst);
            while !RELEASE_PROCESS.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            // SAFETY: the test's buffer has one output channel of `n` samples.
            unsafe { std::slice::from_raw_parts_mut(*outputs, n as usize) }.fill(1.0);
            IN_PROCESS.store(false, Ordering::SeqCst);
            FINISHED_PROCESSES.fetch_add(1, Ordering::SeqCst);
        }

        let effect = fake_effect(dispatch);
        // SAFETY: the test's leaked `AEffect`; nothing else touches it yet.
        unsafe {
            (*effect).processReplacing = Some(render);
            (*effect).numOutputs = 1;
            (*effect).flags = PluginFlags::CAN_REPLACING.bits();
        }
        let (instance, main) = instance_over(effect);
        let mut handle = instance.render_handle(&main);
        let block = |handle: &mut RenderHandle| {
            let mut host: HostBuffer<f32> = HostBuffer::new(0, 1);
            let none: Vec<Vec<f32>> = Vec::new();
            let mut out = vec![vec![9.0f32; 4]];
            handle.process(&mut host.bind(&none, &mut out));
            out
        };

        // Part one: a render is in flight; the name call waits for it.
        let (handle_tx, handle_rx) = std::sync::mpsc::channel();
        let renderer = std::thread::spawn(move || {
            let out = block(&mut handle);
            handle_tx.send(handle).unwrap();
            out
        });
        while !IN_PROCESS.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        let watcher = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(150));
            let early = NAME_DISPATCHED.load(Ordering::SeqCst);
            RELEASE_PROCESS.store(true, Ordering::SeqCst);
            early
        });
        RELEASE_NAME.store(true, Ordering::SeqCst);
        let _ = instance.get_parameter_name(0);
        assert!(
            !watcher.join().unwrap(),
            "the dispatcher call ran while a render was in flight"
        );
        assert!(NAME_DISPATCHED.load(Ordering::SeqCst), "and it ran after");
        assert_eq!(renderer.join().unwrap()[0][0], 1.0);
        let mut handle = handle_rx.recv().unwrap();

        // Part two: a render starts while the name call runs. It is refused.
        NAME_DISPATCHED.store(false, Ordering::SeqCst);
        RELEASE_NAME.store(false, Ordering::SeqCst);
        RELEASE_PROCESS.store(true, Ordering::SeqCst);
        let before = PROCESSES.load(Ordering::SeqCst);
        let during = std::thread::spawn(move || {
            while !IN_NAME.load(Ordering::SeqCst) {
                std::thread::yield_now();
            }
            let out = block(&mut handle);
            RELEASE_NAME.store(true, Ordering::SeqCst);
            out
        });
        let _ = instance.get_parameter_name(0);
        let out = during.join().unwrap();
        assert_eq!(
            PROCESSES.load(Ordering::SeqCst),
            before,
            "a render reached the plugin during a dispatcher call"
        );
        assert!(
            out[0].iter().all(|s| *s == 0.0),
            "and was refused to silence"
        );
    }

    /// **A plugin that raises its pin counts after activation is not rendered into
    /// a buffer sized for the old ones.** Its `processReplacing` indexes the
    /// channel tables by its live `numOutputs`; one more than the host sized is a
    /// read past the table. The handle renders silence instead, and renders again
    /// when the counts fit.
    ///
    /// Mutation (run): put `std::hint::black_box(Box::new(1u8));` in the refusing branch: the
    /// allocation aborts under `assert_no_alloc`. Mutation (run): drop the
    /// `fetch_add` in `RenderHandle::process`: the count assertion fails.
    /// Mutation (run): remove the `pins_fit` checks in `RenderEntry::process` and
    /// `process_f64`: the plugin is called with the larger count (`RENDERS` moves)
    /// and the silence assertions fail.
    #[cfg(unix)]
    #[test]
    fn a_plugin_that_raises_its_pins_is_not_rendered() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static RENDERS: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn dispatch(
            _: *mut AEffect,
            _: i32,
            _: i32,
            _: isize,
            _: *mut c_void,
            _: f32,
        ) -> isize {
            0
        }
        extern "C" fn render_f32(
            _: *mut AEffect,
            _: *const *const f32,
            outputs: *mut *mut f32,
            frames: i32,
        ) {
            RENDERS.fetch_add(1, Ordering::SeqCst);
            // SAFETY: the test's buffer has two output channels of `frames` samples.
            unsafe { std::slice::from_raw_parts_mut(*outputs, frames as usize) }.fill(1.0);
        }
        extern "C" fn render_f64(
            _: *mut AEffect,
            _: *const *const f64,
            outputs: *mut *mut f64,
            frames: i32,
        ) {
            RENDERS.fetch_add(1, Ordering::SeqCst);
            // SAFETY: as for `render_f32`.
            unsafe { std::slice::from_raw_parts_mut(*outputs, frames as usize) }.fill(1.0);
        }

        let effect = fake_effect(dispatch);
        // SAFETY: the test's leaked `AEffect`; the instance reads it, nothing writes
        // concurrently.
        unsafe {
            (*effect).numInputs = 2;
            (*effect).numOutputs = 2;
            (*effect).processReplacing = Some(render_f32);
            (*effect).processReplacingF64 = Some(render_f64);
            (*effect).flags =
                (PluginFlags::CAN_REPLACING | PluginFlags::CAN_DOUBLE_REPLACING).bits();
        }
        let (instance, main) = instance_over(effect);
        let mut render = instance.render_handle(&main);

        let inputs = vec![vec![0.0f32; 8]; 2];
        let mut outputs = vec![vec![9.0f32; 8]; 2];
        let inputs64 = vec![vec![0.0f64; 8]; 2];
        let mut outputs64 = vec![vec![9.0f64; 8]; 2];
        let mut host32: HostBuffer<f32> = HostBuffer::new(2, 2);
        let mut host64: HostBuffer<f64> = HostBuffer::new(2, 2);

        // The counts it was activated with: rendered.
        render.process(&mut host32.bind(&inputs, &mut outputs));
        render.process_f64(&mut host64.bind(&inputs64, &mut outputs64));
        assert_eq!(RENDERS.load(Ordering::SeqCst), 2);
        assert_eq!(outputs[0][0], 1.0);

        // It raises its output pins, as `audioMasterIOChanged` announces.
        // SAFETY: the test's leaked `AEffect`; no other thread.
        unsafe { (*effect).numOutputs = 3 };
        outputs.iter_mut().for_each(|c| c.fill(9.0));
        outputs64.iter_mut().for_each(|c| c.fill(9.0));
        // The refusal is allocation-free and logs nothing: it runs every block on the
        // audio thread, and is counted for the control side instead.
        assert_no_alloc::assert_no_alloc(|| {
            render.process(&mut host32.bind(&inputs, &mut outputs));
            render.process_f64(&mut host64.bind(&inputs64, &mut outputs64));
        });
        assert_eq!(RENDERS.load(Ordering::SeqCst), 2, "called past its buffer");
        assert_eq!(
            instance.pin_refusals(),
            2,
            "both refused blocks are counted"
        );
        assert_eq!(instance.report_pin_refusals(), 2);
        assert!(outputs.iter().flatten().all(|s| *s == 0.0), "f32 is silent");
        assert!(
            outputs64.iter().flatten().all(|s| *s == 0.0),
            "f64 is silent"
        );

        // Fewer pins than the buffer holds is fine.
        // SAFETY: as above.
        unsafe { (*effect).numOutputs = 1 };
        render.process(&mut host32.bind(&inputs, &mut outputs));
        assert_eq!(RENDERS.load(Ordering::SeqCst), 3);
        assert_eq!(instance.pin_refusals(), 2, "a fitting block is not counted");
    }

    /// A loader over `main`, for the entry-point tests.
    #[cfg(unix)]
    fn loader_over(main: PluginMain, id: i32) -> PluginLoader<IdHost> {
        let lib: Library = libloading::os::unix::Library::this().into();
        PluginLoader {
            main,
            lib: Arc::new(lib),
            host: Arc::new(IdHost(id)),
        }
    }

    /// **An `AEffect` that is not one is refused untouched.** A wrong `magic` or a
    /// null `dispatcher` is `InvalidEffect`: nothing is called on it (not even
    /// `effClose`) and `reserved1` is left alone. A valid one loads, and closes
    /// once.
    ///
    /// Mutation (run): drop the `magic`/`dispatcher` check in `instance`: the bad
    /// magic effect is dispatched to (`CALLS` moves) and loads or fails with
    /// another error, and the null dispatcher path reaches the unanswered-opcode
    /// branch; both assertions fail.
    #[cfg(unix)]
    #[test]
    fn an_invalid_aeffect_is_refused_untouched() {
        use std::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        static CLOSES: AtomicUsize = AtomicUsize::new(0);
        static EFFECT: AtomicPtr<AEffect> = AtomicPtr::new(ptr::null_mut());

        extern "C" fn dispatch(
            _: *mut AEffect,
            opcode: i32,
            _: i32,
            _: isize,
            _: *mut c_void,
            _: f32,
        ) -> isize {
            CALLS.fetch_add(1, Ordering::SeqCst);
            if opcode == plugin::OpCode::Shutdown as i32 {
                CLOSES.fetch_add(1, Ordering::SeqCst);
            }
            if opcode == plugin::OpCode::GetApiVersion as i32 {
                return 2400;
            }
            0
        }
        extern "C" fn main_returning_effect(_: crate::api::HostCallbackProc) -> *mut AEffect {
            EFFECT.load(Ordering::SeqCst)
        }

        // A wrong magic.
        let effect = fake_effect(dispatch);
        // SAFETY: the test's leaked `AEffect`.
        unsafe { (*effect).magic = 0x1234 };
        EFFECT.store(effect, Ordering::SeqCst);
        let result = loader_over(main_returning_effect, 1).instance(&MainThread::claim().unwrap());
        assert!(matches!(result, Err(PluginLoadError::InvalidEffect)));
        assert_eq!(CALLS.load(Ordering::SeqCst), 0, "a bad AEffect was called");
        // SAFETY: as above.
        assert_eq!(unsafe { (*effect).reserved1 }, 0, "reserved1 was written");

        // No dispatcher.
        let effect = fake_effect(dispatch);
        // SAFETY: as above.
        unsafe { (*effect).dispatcher = None };
        EFFECT.store(effect, Ordering::SeqCst);
        let result = loader_over(main_returning_effect, 1).instance(&MainThread::claim().unwrap());
        assert!(matches!(result, Err(PluginLoadError::InvalidEffect)));
        assert_eq!(CALLS.load(Ordering::SeqCst), 0);

        // A valid one.
        let effect = fake_effect(dispatch);
        EFFECT.store(effect, Ordering::SeqCst);
        let instance = loader_over(main_returning_effect, 1)
            .instance(&MainThread::claim().unwrap())
            .expect("a valid AEffect loads");
        assert_eq!(CLOSES.load(Ordering::SeqCst), 0);
        drop(instance);
        assert_eq!(CLOSES.load(Ordering::SeqCst), 1);
    }

    /// **A callback during `main` reaches the loader's host whatever the plugin
    /// left in `AEffect::reserved1`.** The field is "reserved for host", and a
    /// plugin that does not zero its `AEffect` before calling back from `main`
    /// leaves garbage in it, which was dereferenced as the host box.
    ///
    /// Mutation (run): check `reserved1` before `LOAD_HOST` again: the garbage
    /// pointer is dereferenced and the test aborts on a signal.
    #[cfg(unix)]
    #[test]
    fn a_callback_in_main_ignores_a_garbage_reserved1() {
        extern "C" fn garbage_main(callback: crate::api::HostCallbackProc) -> *mut AEffect {
            // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
            let mut effect: AEffect = unsafe { std::mem::zeroed() };
            effect.reserved1 = 0xdead_beef;
            // SAFETY: `callback` is `callback_wrapper`; opcode 2 is `audioMasterCurrentId`.
            let answer = unsafe { callback(&mut effect, 2, 0, 0, ptr::null_mut(), 0.0) };
            ANSWERED.set(answer);
            ptr::null_mut()
        }
        let mut loader = loader_over(garbage_main, 7);
        assert!(loader.call_main().is_null());
        assert_eq!(
            ANSWERED.get(),
            7,
            "the call did not reach the loader's host"
        );
    }

    /// **A thread the plugin spawns during `main` cannot make the host dereference a
    /// garbage `reserved1`.** `LOAD_HOST` is thread-local, so such a thread has none;
    /// with a load in progress it gets the load-time default (`0`) unless the effect it
    /// passes is one the host adopted, in which case it reaches that effect's own host
    /// (an adopted plugin's audio thread keeps working through someone else's load).
    ///
    /// Mutation (run): drop the registry check in `callback_wrapper` (trust `reserved1`
    /// whenever it is non-zero): the garbage pointer is dereferenced and the test aborts.
    /// Mutation (run): drop the `adopt` call in `instance`: the adopted effect's call is
    /// answered `0` instead of its host's, and the second assertion fails.
    #[cfg(unix)]
    #[test]
    fn a_spawned_thread_during_main_cannot_reach_a_garbage_reserved1() {
        use std::sync::atomic::{AtomicIsize, AtomicPtr, AtomicUsize, Ordering};
        static GARBAGE_ANSWER: AtomicIsize = AtomicIsize::new(-1);
        static ADOPTED_ANSWER: AtomicIsize = AtomicIsize::new(-1);
        static OTHER: AtomicPtr<AEffect> = AtomicPtr::new(ptr::null_mut());
        static CALLS: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn dispatch(
            _: *mut AEffect,
            opcode: i32,
            _: i32,
            _: isize,
            _: *mut c_void,
            _: f32,
        ) -> isize {
            if opcode == plugin::OpCode::GetApiVersion as i32 {
                return 2400;
            }
            0
        }
        extern "C" fn first_main(_: crate::api::HostCallbackProc) -> *mut AEffect {
            CALLS.fetch_add(1, Ordering::SeqCst);
            fake_effect(dispatch)
        }
        // A plugin whose own thread calls the host while `main` runs: once with an
        // `AEffect` of garbage, once with the first plugin's adopted effect.
        extern "C" fn second_main(callback: crate::api::HostCallbackProc) -> *mut AEffect {
            let other = OTHER.load(Ordering::SeqCst) as usize;
            std::thread::spawn(move || {
                // SAFETY: `AEffect` is `repr(C)` of integers, floats, raw pointers and `Option` fn pointers, all valid when zero (null pointers, `None`).
                let mut garbage: AEffect = unsafe { std::mem::zeroed() };
                garbage.reserved1 = 0xdead_beef;
                // SAFETY: `callback` is `callback_wrapper`; opcode 2 is `audioMasterCurrentId`.
                let a = unsafe { callback(&mut garbage, 2, 0, 0, ptr::null_mut(), 0.0) };
                GARBAGE_ANSWER.store(a, Ordering::SeqCst);
                // SAFETY: as above, with an effect the first load adopted.
                let b = unsafe { callback(other as *mut AEffect, 2, 0, 0, ptr::null_mut(), 0.0) };
                ADOPTED_ANSWER.store(b, Ordering::SeqCst);
            })
            .join()
            .unwrap();
            ptr::null_mut()
        }

        let first = loader_over(first_main, 5)
            .instance(&MainThread::claim().unwrap())
            .expect("the first plugin loads");
        OTHER.store(first.core.get_effect(), Ordering::SeqCst);
        let second = loader_over(second_main, 9).instance(&MainThread::claim().unwrap());
        assert!(matches!(second, Err(PluginLoadError::InstanceFailed)));
        assert_eq!(
            GARBAGE_ANSWER.load(Ordering::SeqCst),
            0,
            "a garbage effect got a host's answer"
        );
        assert_eq!(
            ADOPTED_ANSWER.load(Ordering::SeqCst),
            5,
            "an adopted effect did not reach its own host"
        );
        drop(first);
    }

    /// **A callback from inside a render takes no lock and allocates nothing, even
    /// while another plugin loads.** The render entry marks the effect it is calling
    /// (`RENDERING`), so `callback_wrapper` does not consult the registry mutex for it.
    ///
    /// Mutation (run): drop the `effect != RENDERING.get()` term in `callback_wrapper`:
    /// the callback takes the registry lock and `REGISTRY_LOCKS` moves, so the second
    /// assertion fails.
    #[cfg(unix)]
    #[test]
    fn a_callback_from_a_render_takes_no_lock_during_a_load() {
        use std::sync::atomic::{AtomicIsize, AtomicUsize, Ordering};
        static ANSWER: AtomicIsize = AtomicIsize::new(-1);
        static RENDERS: AtomicUsize = AtomicUsize::new(0);

        extern "C" fn dispatch(
            _: *mut AEffect,
            opcode: i32,
            _: i32,
            _: isize,
            _: *mut c_void,
            _: f32,
        ) -> isize {
            if opcode == plugin::OpCode::GetApiVersion as i32 {
                return 2400;
            }
            0
        }
        extern "C" fn render(effect: *mut AEffect, _: *const *const f32, _: *mut *mut f32, _: i32) {
            RENDERS.fetch_add(1, Ordering::SeqCst);
            // The plugin asks the host something from inside `processReplacing`:
            // opcode 2 is `audioMasterCurrentId`.
            ANSWER.store(
                callback_wrapper::<IdHost>(effect, 2, 0, 0, ptr::null_mut(), 0.0),
                Ordering::SeqCst,
            );
        }
        extern "C" fn main_fn(_: crate::api::HostCallbackProc) -> *mut AEffect {
            let effect = fake_effect(dispatch);
            // SAFETY: the test's leaked `AEffect`.
            unsafe {
                (*effect).processReplacing = Some(render);
                (*effect).flags = PluginFlags::CAN_REPLACING.bits();
            }
            effect
        }

        let instance = loader_over(main_fn, 5)
            .instance(&MainThread::claim().unwrap())
            .expect("loads");
        let mut handle = instance.render_handle(&MainThread::claim().unwrap());
        let mut host32: HostBuffer<f32> = HostBuffer::new(0, 0);
        let none: Vec<Vec<f32>> = Vec::new();
        let mut none_out: Vec<Vec<f32>> = Vec::new();

        // Another plugin is loading.
        let _loading = Loading::begin();
        let before = REGISTRY_LOCKS.load(Ordering::SeqCst);
        assert_no_alloc::assert_no_alloc(|| {
            handle.process(&mut host32.bind(&none, &mut none_out));
        });
        assert_eq!(RENDERS.load(Ordering::SeqCst), 1);
        assert_eq!(
            ANSWER.load(Ordering::SeqCst),
            5,
            "the callback was not answered"
        );
        assert_eq!(
            REGISTRY_LOCKS.load(Ordering::SeqCst),
            before,
            "the render path took the registry lock"
        );
    }

    /// **A string sent to the plugin is a C string that fits the opcode's buffer.**
    /// A program name longer than `kVstMaxProgNameLen` (24 with the NUL) is cut to
    /// 23 bytes at a char boundary, and an interior NUL cuts it (and no longer
    /// panics through `expect`). The fake records the C string it is handed.
    ///
    /// Mutation (run): pass `usize::MAX` as the program name's `max_len`: the
    /// 100-byte name reaches the plugin whole and the first assertion fails.
    /// Mutation (run): `CString::new(string).expect(..)` on the raw string again:
    /// the `"a\0b"` call panics.
    #[test]
    fn strings_to_the_plugin_are_short_and_c_strings() {
        use std::sync::Mutex;
        static SEEN: Mutex<Vec<u8>> = Mutex::new(Vec::new());

        extern "C" fn dispatch(
            _: *mut AEffect,
            opcode: i32,
            _: i32,
            _: isize,
            ptr: *mut c_void,
            _: f32,
        ) -> isize {
            if opcode == plugin::OpCode::SetCurrentPresetName as i32 && !ptr.is_null() {
                // SAFETY: the host passes a NUL-terminated string (the property
                // under test); the read stops at the NUL.
                let bytes = unsafe { std::ffi::CStr::from_ptr(ptr as *const _) }.to_bytes();
                *SEEN.lock().unwrap() = bytes.to_vec();
            }
            0
        }
        let effect = fake_effect(dispatch);
        let (params, main) = instance_over(effect);

        // `kVstMaxProgNameLen` is 24 with the NUL: 23 bytes reach the plugin.
        params.set_preset_name(&main, "n".repeat(100));
        assert_eq!(SEEN.lock().unwrap().len(), MAX_PRESET_NAME_LEN - 1);

        // A multi-byte char is not cut in half.
        params.set_preset_name(&main, "é".repeat(100));
        let seen = SEEN.lock().unwrap().clone();
        assert!(std::str::from_utf8(&seen).is_ok(), "cut inside a char");
        assert!(seen.len() < MAX_PRESET_NAME_LEN);

        // An interior NUL cuts the string; it does not panic.
        params.set_preset_name(&main, "a\0b".to_string());
        assert_eq!(*SEEN.lock().unwrap(), b"a");
    }

    /// `truncated_c_str`, directly: the NUL cut, the cap, and a char boundary.
    ///
    /// Mutation (run): drop the `is_char_boundary` loop: the `"é"` case slices
    /// inside the char and panics.
    #[test]
    fn truncated_c_str_cuts_at_a_nul_a_cap_and_a_char_boundary() {
        assert_eq!(truncated_c_str("abc", 10), "abc");
        assert_eq!(truncated_c_str("abc", 2), "ab");
        assert_eq!(truncated_c_str("a\0bc", 10), "a");
        assert_eq!(truncated_c_str("\0abc", 10), "");
        assert_eq!(truncated_c_str("aé", 2), "a");
        assert_eq!(truncated_c_str("aé", 3), "aé");
        assert_eq!(truncated_c_str("", 0), "");
    }

    /// **A plugin that writes through `effSetChunk`'s pointer writes into a copy.**
    /// The opcode's pointer is `void*`; the caller's slice must not be handed over.
    ///
    /// Mutation (run): pass `data.as_ptr() as *mut c_void` in `set_chunk` again:
    /// the plugin's write lands in the caller's bytes and the assertion fails.
    #[test]
    fn a_chunk_a_plugin_writes_into_is_a_copy() {
        extern "C" fn dispatch(
            _: *mut AEffect,
            opcode: i32,
            _: i32,
            value: isize,
            ptr: *mut c_void,
            _: f32,
        ) -> isize {
            if opcode == plugin::OpCode::SetData as i32 {
                // SAFETY: the host gives `value` writable bytes (the property under
                // test: a copy it owns).
                unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, value as usize) }
                    .fill(0xEE);
                return 1;
            }
            0
        }
        let (params, main) = instance_over(fake_effect(dispatch));
        let data = vec![1u8, 2, 3, 4];
        assert!(params.load_preset_data(&main, &data));
        assert!(params.load_bank_data(&main, &data));
        assert_eq!(
            data,
            vec![1, 2, 3, 4],
            "the plugin wrote into the caller's bytes"
        );
    }

    /// A name a plugin writes past the spec's length (real plugins do) lands in a
    /// buffer that holds it, and comes back whole.
    ///
    /// Mutation (run): size the buffer by `max` alone in `read_string_param`: the
    /// plugin overruns the heap block (undefined behaviour; under the allocator in
    /// use the read of the first 32 bytes returns a 32-byte name) and the length
    /// assertion fails.
    #[test]
    fn a_name_longer_than_the_spec_fits_the_buffer() {
        extern "C" fn dispatch(
            _: *mut AEffect,
            opcode: i32,
            _: i32,
            _: isize,
            ptr: *mut c_void,
            _: f32,
        ) -> isize {
            if opcode == plugin::OpCode::GetParameterName as i32 {
                // SAFETY: the host offers at least 256 bytes (the property under test).
                let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, 200) };
                buf.fill(b'x');
            }
            0
        }
        let (params, _main) = instance_over(fake_effect(dispatch));
        assert_eq!(params.get_parameter_name(0).len(), 200);
    }
}
