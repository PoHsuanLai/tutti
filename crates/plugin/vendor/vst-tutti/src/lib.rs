// Forked from vst-rs 0.3.0. This is *owned* code, not a read-only vendor drop:
// the fork already diverges deliberately (host-side audioMaster callbacks, plus
// the host-safety fixes documented in `host.rs`), so its lints are chased like
// any other crate here. The blanket `allow(clippy::all)` that used to sit at the
// top of this file is what kept those bugs invisible.
//
// The remaining allows are narrow and each has a reason:
#![allow(missing_docs)]
// Elided-lifetime style across a 2021-edition port of a 2018-edition crate.
#![allow(mismatched_lifetime_syntaxes)]
//
// `allow(useless_ptr_null_checks)` used to sit here, justified as "the null
// checks on FFI function pointers in `host.rs` are load-bearing even though
// rustc believes an `extern "C" fn` is non-null by type". rustc was right and
// the justification was backwards: the checks compiled to `false` under `-O`,
// so the guard shipped only in debug builds while release jumped to address 0.
// The nullable slots are `Option<...>` now and the lint has nothing left
// to fire on — if it ever fires again, that is a real bug, not noise.

//! A rust implementation of the VST2.4 API.
//!
//! The VST API is multi-threaded. A VST host calls into a plugin generally from two threads -
//! the *processing* thread and the *UI* thread. The organization of this crate reflects this
//! structure to ensure that the threading assumptions of Safe Rust are fulfilled and data
//! races are avoided.
//!
//! # Plugins
//! All Plugins must implement the `Plugin` trait and `std::default::Default`.
//! The `plugin_main!` macro must also be called in order to export the necessary functions
//! for the plugin to function.
//!
//! ## `Plugin` Trait
//! All methods in this trait have a default implementation except for the `get_info` method which
//! must be implemented by the plugin. Any of the default implementations may be overridden for
//! custom functionality; the defaults do nothing on their own.
//!
//! ## `PluginParameters` Trait
//! The methods in this trait handle access to plugin parameters. Since the host may call these
//! methods concurrently with audio processing, it needs to be separate from the main `Plugin`
//! trait.
//!
//! To support parameters, a plugin must provide an implementation of the `PluginParameters`
//! trait, wrap it in an `Arc` (so it can be accessed from both threads) and
//! return a reference to it from the `get_parameter_object` method in the `Plugin`.
//!
//! ## `plugin_main!` macro
//! `plugin_main!` will export the necessary functions to create a proper VST plugin. This must be
//! called with your VST plugin struct name in order for the vst to work.
//!
//! ## Example plugin
//! A barebones VST plugin:
//!
//! ```no_run
//! #[macro_use]
//! extern crate vst_tutti as vst;
//!
//! use vst_tutti::plugin::{HostCallback, Info, Plugin};
//!
//! struct BasicPlugin;
//!
//! impl Plugin for BasicPlugin {
//!     fn new(_host: HostCallback) -> Self {
//!         BasicPlugin
//!     }
//!
//!     fn get_info(&self) -> Info {
//!         Info {
//!             name: "Basic Plugin".to_string(),
//!             unique_id: 1357, // Used by hosts to differentiate between plugins.
//!
//!             ..Default::default()
//!         }
//!     }
//! }
//!
//! plugin_main!(BasicPlugin); // Important!
//! # fn main() {} // For `extern crate vst`
//! ```
//!
//! # Hosts
//!
//! ## `Host` Trait
//! All hosts must implement the [`Host` trait](host/trait.Host.html). To load a VST plugin, you
//! need to wrap your host in an `Arc<T>` wrapper. Along with the
//! plugin path, this can be passed to the [`PluginLoader::load`] method to create a plugin loader
//! which can spawn plugin instances.
//!
//! ## Example Host
//! ```no_run
//! extern crate vst_tutti as vst;
//!
//! use std::sync::Arc;
//! use std::path::Path;
//!
//! use vst_tutti::host::{Host, PluginLoader};
//! use vst_tutti::plugin::Plugin;
//!
//! struct SampleHost;
//!
//! impl Host for SampleHost {
//!     fn automate(&self, index: i32, value: f32) {
//!         println!("Parameter {} had its value changed to {}", index, value);
//!     }
//! }
//!
//! fn main() {
//!     let host = Arc::new(SampleHost);
//!     let path = Path::new("/path/to/vst");
//!
//!     let mut loader = PluginLoader::load(path, host.clone()).unwrap();
//!     let mut instance = loader.instance().unwrap();
//!
//!     println!("Loaded {}", instance.get_info().name);
//!
//!     instance.init();
//!     println!("Initialized instance!");
//!
//!     println!("Closing instance...");
//!     // Not necessary as the instance is shut down when it goes out of scope anyway.
//!     // drop(instance);
//! }
//!
//! ```
//!
//! [`PluginLoader::load`]: host/struct.PluginLoader.html#method.load
//!

extern crate libc;
extern crate libloading;
extern crate num_enum;
extern crate num_traits;
#[macro_use]
extern crate log;
#[macro_use]
extern crate bitflags;

use std::ptr;

pub mod api;
pub mod buffer;
mod cache;
pub mod channels;
pub mod editor;
pub mod event;
pub mod host;
mod interfaces;
pub mod plugin;
pub mod prelude;
pub mod util;

use api::consts::VST_MAGIC;
use api::{AEffect, HostCallbackProc};
use cache::PluginCache;
use plugin::{HostCallback, Plugin};

/// Exports the necessary symbols for the plugin to be used by a VST host.
///
/// This macro takes a type which must implement the `Plugin` trait.
#[macro_export]
macro_rules! plugin_main {
    ($t:ty) => {
        #[cfg(target_os = "macos")]
        #[no_mangle]
        pub extern "system" fn main_macho(
            callback: $crate::api::HostCallbackProc,
        ) -> *mut $crate::api::AEffect {
            VSTPluginMain(callback)
        }

        #[cfg(target_os = "windows")]
        #[allow(non_snake_case)]
        #[no_mangle]
        pub extern "system" fn MAIN(
            callback: $crate::api::HostCallbackProc,
        ) -> *mut $crate::api::AEffect {
            VSTPluginMain(callback)
        }

        #[allow(non_snake_case)]
        #[no_mangle]
        pub extern "C" fn VSTPluginMain(
            callback: $crate::api::HostCallbackProc,
        ) -> *mut $crate::api::AEffect {
            $crate::main::<$t>(callback)
        }
    };
}

/// Initializes a VST plugin and returns a raw pointer to an AEffect struct.
#[doc(hidden)]
pub fn main<T: Plugin>(callback: HostCallbackProc) -> *mut AEffect {
    // Initialize as much of the AEffect as we can before creating the plugin.
    // In particular, initialize all the function pointers, since initializing
    // these to zero is undefined behavior.
    let boxed_effect = Box::new(AEffect {
        magic: VST_MAGIC,
        dispatcher: Some(interfaces::dispatch), // fn pointer

        _process: Some(interfaces::process_deprecated), // fn pointer

        setParameter: Some(interfaces::set_parameter),
        getParameter: Some(interfaces::get_parameter),

        numPrograms: 0, // To be updated with plugin specific value.
        numParams: 0,   // To be updated with plugin specific value.
        numInputs: 0,   // To be updated with plugin specific value.
        numOutputs: 0,  // To be updated with plugin specific value.

        flags: 0, // To be updated with plugin specific value.

        reserved1: 0,
        reserved2: 0,

        initialDelay: 0, // To be updated with plugin specific value.

        _realQualities: 0,
        _offQualities: 0,
        _ioRatio: 0.0,

        object: ptr::null_mut(),
        user: ptr::null_mut(),

        uniqueId: 0, // To be updated with plugin specific value.
        version: 0,  // To be updated with plugin specific value.

        processReplacing: Some(interfaces::process_replacing), // fn pointer
        processReplacingF64: Some(interfaces::process_replacing_f64), //fn pointer

        future: [0u8; 56],
    });
    let raw_effect = Box::into_raw(boxed_effect);

    let host = HostCallback::wrap(callback, raw_effect);
    if host.vst_version() == 0 {
        // TODO: Better criteria would probably be useful here...
        return ptr::null_mut();
    }

    trace!("Creating VST plugin instance...");
    let mut plugin = T::new(host);
    let info = plugin.get_info();
    let params = plugin.get_parameter_object();
    let editor = plugin.get_editor();

    // Update AEffect in place
    let effect = unsafe { &mut *raw_effect };
    effect.numPrograms = info.presets;
    effect.numParams = info.parameters;
    effect.numInputs = info.inputs;
    effect.numOutputs = info.outputs;
    effect.flags = {
        use api::PluginFlags;

        let mut flag = PluginFlags::CAN_REPLACING;

        if info.f64_precision {
            flag |= PluginFlags::CAN_DOUBLE_REPLACING;
        }

        if editor.is_some() {
            flag |= PluginFlags::HAS_EDITOR;
        }

        if info.preset_chunks {
            flag |= PluginFlags::PROGRAM_CHUNKS;
        }

        if let plugin::Category::Synth = info.category {
            flag |= PluginFlags::IS_SYNTH;
        }

        if info.silent_when_stopped {
            flag |= PluginFlags::NO_SOUND_IN_STOP;
        }

        flag.bits()
    };
    effect.initialDelay = info.initial_delay;
    effect.object = Box::into_raw(Box::new(Box::new(plugin) as Box<dyn Plugin>)) as *mut _;
    effect.user = Box::into_raw(Box::new(PluginCache::new(&info, params, editor))) as *mut _;
    effect.uniqueId = info.unique_id;
    effect.version = info.version;

    effect
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use std::os::raw::c_void;

    use crate::{
        api::{consts::VST_MAGIC, AEffect},
        interfaces,
        plugin::{HostCallback, Info, Plugin},
    };

    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestPlugin;

    /// Minimal editor, so `effEditGetRect` reaches its dispatch arm.
    ///
    /// Without an editor that opcode returns 0 through the `if let Some(..)`
    /// and never touches the rect at all — the arm would be unreachable from
    /// any test here.
    struct TestEditor;

    impl crate::editor::Editor for TestEditor {
        fn size(&self) -> (i32, i32) {
            (320, 240)
        }

        fn position(&self) -> (i32, i32) {
            (10, 20)
        }

        fn open(&mut self, _parent: *mut c_void) -> bool {
            false
        }

        fn close(&mut self) {}

        fn is_open(&mut self) -> bool {
            false
        }
    }

    impl Plugin for TestPlugin {
        fn new(_host: HostCallback) -> Self {
            TestPlugin
        }

        fn get_editor(&mut self) -> Option<Box<dyn crate::editor::Editor>> {
            Some(Box::new(TestEditor))
        }

        fn get_info(&self) -> Info {
            Info {
                name: "Test Plugin".to_string(),
                vendor: "overdrivenpotato".to_string(),

                presets: 1,
                parameters: 1,

                unique_id: 5678,
                version: 1234,

                initial_delay: 123,

                ..Default::default()
            }
        }
    }

    plugin_main!(TestPlugin);

    extern "C" fn pass_callback(
        _effect: *mut AEffect,
        _opcode: i32,
        _index: i32,
        _value: isize,
        _ptr: *mut c_void,
        _opt: f32,
    ) -> isize {
        1
    }

    extern "C" fn fail_callback(
        _effect: *mut AEffect,
        _opcode: i32,
        _index: i32,
        _value: isize,
        _ptr: *mut c_void,
        _opt: f32,
    ) -> isize {
        0
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn old_hosts() {
        assert_eq!(MAIN(fail_callback), ptr::null_mut());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn old_hosts() {
        assert_eq!(main_macho(fail_callback), ptr::null_mut());
    }

    #[test]
    fn host_callback() {
        assert_eq!(VSTPluginMain(fail_callback), ptr::null_mut());
    }

    #[test]
    fn aeffect_created() {
        let aeffect = VSTPluginMain(pass_callback);
        assert!(!aeffect.is_null());
    }

    /// Set by `TestPlugin::drop`, and read by `plugin_drop`.
    ///
    /// A counter rather than a flag, and module-level rather than nested inside
    /// the one test that reads it: an `impl` is never scoped, so writing
    /// `impl Drop for TestPlugin` inside `plugin_drop` still applied it
    /// crate-wide — `plugin_no_drop` drops a `TestPlugin` too, and a shared
    /// `bool` made the pair order-dependent. Comparing a before/after count
    /// keeps each test reading only its own drop.
    static DROPS: AtomicUsize = AtomicUsize::new(0);

    impl Drop for TestPlugin {
        fn drop(&mut self) {
            DROPS.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn plugin_drop() {
        let before = DROPS.load(Ordering::SeqCst);

        let aeffect = VSTPluginMain(pass_callback);
        assert!(!aeffect.is_null());

        unsafe { (*aeffect).drop_plugin() };

        // Assert that the VST is shut down and dropped.
        assert_eq!(
            DROPS.load(Ordering::SeqCst),
            before + 1,
            "drop_plugin must run the plugin's destructor exactly once"
        );
    }

    #[test]
    fn plugin_no_drop() {
        let aeffect = VSTPluginMain(pass_callback);
        assert!(!aeffect.is_null());

        // Make sure this doesn't crash.
        unsafe { (*aeffect).drop_plugin() };
    }

    #[test]
    fn plugin_deref() {
        let aeffect = VSTPluginMain(pass_callback);
        assert!(!aeffect.is_null());

        let plugin = unsafe { (*aeffect).get_plugin() };
        // Assert that deref works correctly.
        assert!(plugin.get_info().name == "Test Plugin");
    }

    #[test]
    fn aeffect_params() {
        // Every function slot in `AEffect` is now `Option`, so each needs
        // unwrapping before the address comparison — and asserting `Some` is
        // itself part of the contract: we must install these, whatever a
        // third-party plugin does.
        // Both sides go through `as *const ()` before `as usize`: casting a
        // function *item* straight to an integer is what `function_casts_as_integer`
        // flags, and the pointer step is what the comparison already meant.
        macro_rules! assert_opt_fn_eq {
            ($a:expr, $b:expr) => {
                assert_eq!(
                    $a.expect("this crate must install the entry point") as *const () as usize,
                    $b as *const () as usize
                );
            };
        }

        let aeffect = unsafe { &mut *VSTPluginMain(pass_callback) };

        assert_eq!(aeffect.magic, VST_MAGIC);
        assert_opt_fn_eq!(aeffect.dispatcher, interfaces::dispatch);
        assert_opt_fn_eq!(aeffect._process, interfaces::process_deprecated);
        assert_opt_fn_eq!(aeffect.setParameter, interfaces::set_parameter);
        assert_opt_fn_eq!(aeffect.getParameter, interfaces::get_parameter);
        assert_eq!(aeffect.numPrograms, 1);
        assert_eq!(aeffect.numParams, 1);
        assert_eq!(aeffect.numInputs, 2);
        assert_eq!(aeffect.numOutputs, 2);
        assert_eq!(aeffect.reserved1, 0);
        assert_eq!(aeffect.reserved2, 0);
        assert_eq!(aeffect.initialDelay, 123);
        assert_eq!(aeffect.uniqueId, 5678);
        assert_eq!(aeffect.version, 1234);
        assert_opt_fn_eq!(aeffect.processReplacing, interfaces::process_replacing);
        assert_opt_fn_eq!(
            aeffect.processReplacingF64,
            interfaces::process_replacing_f64
        );
    }

    /// `effEditGetRect` hands back the same buffer every time.
    ///
    /// The opcode's `Rect**` out-parameter has no companion "free this"
    /// opcode, and the host cannot know the plugin's allocator — so whatever it
    /// is pointed at must be owned by the plugin and outlive the call. A
    /// `Box::into_raw` per call satisfies the lifetime and leaks a `Rect` every
    /// time, and hosts call this repeatedly: once before opening an editor, and
    /// again on each resize.
    ///
    /// Pointer identity is the observable form of that: a fresh allocation
    /// gives a different address, a reused buffer gives the same one.
    #[test]
    fn edit_get_rect_reuses_one_buffer_rather_than_leaking_per_call() {
        use crate::plugin::OpCode;

        let aeffect = unsafe { &mut *VSTPluginMain(pass_callback) };
        let dispatch = aeffect.dispatcher.expect("dispatcher must be installed");

        let mut first: *mut c_void = ptr::null_mut();
        let mut second: *mut c_void = ptr::null_mut();

        // SAFETY: `dispatch` is this crate's own `interfaces::dispatch`, and
        // `aeffect` is the instance it was installed on. The out-param is a
        // live `*mut *mut c_void` matching what `EditorGetRect` writes.
        let ok = unsafe {
            dispatch(
                aeffect,
                OpCode::EditorGetRect.into(),
                0,
                0,
                &mut first as *mut _ as *mut c_void,
                0.0,
            )
        };
        assert_eq!(ok, 1, "the test plugin has an editor, so this must succeed");
        assert!(
            !first.is_null(),
            "a successful get-rect must write a pointer"
        );

        // SAFETY: as above — same effect, same opcode, second out-param.
        unsafe {
            dispatch(
                aeffect,
                OpCode::EditorGetRect.into(),
                0,
                0,
                &mut second as *mut _ as *mut c_void,
                0.0,
            );
        }

        assert_eq!(
            first, second,
            "each call handed back a fresh allocation, so every get-rect leaks \
             one Rect — and nothing in VST 2.4 lets the host free it"
        );
    }

    /// The rect carries the editor's origin as well as its extent.
    ///
    /// `effEditGetRect` reports `left`/`top`/`right`/`bottom`, so the extent is
    /// a *difference* — a plugin whose editor sits at a non-zero origin has a
    /// right edge past its width. Writing the size into the origin fields would
    /// still produce the correct width for the common `(0, 0)` case, which is
    /// why the test editor sits at `(10, 20)`.
    #[test]
    fn edit_get_rect_reports_origin_and_extent() {
        use crate::editor::Rect;
        use crate::plugin::OpCode;

        let aeffect = unsafe { &mut *VSTPluginMain(pass_callback) };
        let dispatch = aeffect.dispatcher.expect("dispatcher must be installed");

        let mut out: *mut Rect = ptr::null_mut();
        // SAFETY: as above; `out` receives the plugin's own `Rect`.
        unsafe {
            dispatch(
                aeffect,
                OpCode::EditorGetRect.into(),
                0,
                0,
                &mut out as *mut _ as *mut c_void,
                0.0,
            );
        }
        assert!(!out.is_null());

        let rect = unsafe { *out };
        assert_eq!((rect.left, rect.top), (10, 20), "origin");
        assert_eq!(
            (rect.right - rect.left, rect.bottom - rect.top),
            (320, 240),
            "extent is the difference between the edges, not the edges"
        );
    }
}
