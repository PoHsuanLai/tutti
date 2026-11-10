//! The plugin-authoring half of the old `vst-tutti` crate, for the test plugins.
//!
//! `tutti-vst2-ffi` is the host half (the ABI types, the loader, the host callback,
//! the buffers). This crate is what a VST2 *plugin* needs on top: `plugin_main!`,
//! `main` (which builds the `AEffect`), the plugin-side dispatcher, `PluginCache`
//! and the `util` helpers. It re-exports the ffi crate's modules, so a plugin
//! names `vst::api`, `vst::plugin::Plugin`, ... from one place. Derived from vst-rs
//! 0.3.0 (MIT, see `LICENSE`).
//!
//! It is a test-only crate (`publish = false`): the only plugin the engine ships
//! tests against is the reference plugin built on it.
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]
#![allow(missing_docs)]
#![allow(mismatched_lifetime_syntaxes)]

#[macro_use]
extern crate log;

use std::ptr;

pub use tutti_vst2_ffi::{api, buffer, channels, editor, event, host, plugin};

mod aeffect;
mod cache;
mod dispatch;
#[cfg(test)]
mod plugin_tests;
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
        #[unsafe(no_mangle)]
        pub extern "system" fn main_macho(
            callback: $crate::api::HostCallbackProc,
        ) -> *mut $crate::api::AEffect {
            VSTPluginMain(callback)
        }

        #[cfg(target_os = "windows")]
        #[allow(non_snake_case)]
        #[unsafe(no_mangle)]
        pub extern "system" fn MAIN(
            callback: $crate::api::HostCallbackProc,
        ) -> *mut $crate::api::AEffect {
            VSTPluginMain(callback)
        }

        #[allow(non_snake_case)]
        #[unsafe(no_mangle)]
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
        dispatcher: Some(dispatch::dispatch), // fn pointer

        _process: Some(dispatch::process_deprecated), // fn pointer

        setParameter: Some(dispatch::set_parameter),
        getParameter: Some(dispatch::get_parameter),

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

        processReplacing: Some(dispatch::process_replacing), // fn pointer
        processReplacingF64: Some(dispatch::process_replacing_f64), //fn pointer

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
    // SAFETY: `raw_effect` came from `Box::into_raw` just above, so it is non-null,
    // aligned and uniquely owned here.
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

    use crate::aeffect::AEffectExt as _;
    use crate::{
        api::{AEffect, consts::VST_MAGIC},
        dispatch,
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

        // SAFETY: `aeffect` was installed by this library and is dropped exactly once.
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
        // SAFETY: `aeffect` was installed by this library and is dropped exactly once.
        unsafe { (*aeffect).drop_plugin() };
    }

    #[test]
    fn plugin_deref() {
        let aeffect = VSTPluginMain(pass_callback);
        assert!(!aeffect.is_null());

        // SAFETY: `aeffect` was installed by this library and nothing else accesses it.
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

        // SAFETY: `VSTPluginMain` returns the leaked `Box<AEffect>`; `pass_callback`
        // reports a VST version, so it is non-null and uniquely used by this test.
        let aeffect = unsafe { &mut *VSTPluginMain(pass_callback) };

        assert_eq!(aeffect.magic, VST_MAGIC);
        assert_opt_fn_eq!(aeffect.dispatcher, dispatch::dispatch);
        assert_opt_fn_eq!(aeffect._process, dispatch::process_deprecated);
        assert_opt_fn_eq!(aeffect.setParameter, dispatch::set_parameter);
        assert_opt_fn_eq!(aeffect.getParameter, dispatch::get_parameter);
        assert_eq!(aeffect.numPrograms, 1);
        assert_eq!(aeffect.numParams, 1);
        assert_eq!(aeffect.numInputs, 2);
        assert_eq!(aeffect.numOutputs, 2);
        assert_eq!(aeffect.reserved1, 0);
        assert_eq!(aeffect.reserved2, 0);
        assert_eq!(aeffect.initialDelay, 123);
        assert_eq!(aeffect.uniqueId, 5678);
        assert_eq!(aeffect.version, 1234);
        assert_opt_fn_eq!(aeffect.processReplacing, dispatch::process_replacing);
        assert_opt_fn_eq!(aeffect.processReplacingF64, dispatch::process_replacing_f64);
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

        // SAFETY: `VSTPluginMain` returns the leaked `Box<AEffect>`; `pass_callback`
        // reports a VST version, so it is non-null and uniquely used by this test.
        let aeffect = unsafe { &mut *VSTPluginMain(pass_callback) };
        let dispatch = aeffect.dispatcher.expect("dispatcher must be installed");

        let mut first: *mut c_void = ptr::null_mut();
        let mut second: *mut c_void = ptr::null_mut();

        // SAFETY: `dispatch` is this crate's own `dispatch::dispatch`, and
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

        // SAFETY: `VSTPluginMain` returns the leaked `Box<AEffect>`; `pass_callback`
        // reports a VST version, so it is non-null and uniquely used by this test.
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

        // SAFETY: `out` is non-null (asserted) and points at the plugin's thread-local
        // `Rect`, written on this thread and still live.
        let rect = unsafe { *out };
        assert_eq!((rect.left, rect.top), (10, 20), "origin");
        assert_eq!(
            (rect.right - rect.left, rect.bottom - rect.top),
            (320, 240),
            "extent is the difference between the edges, not the edges"
        );
    }
}
