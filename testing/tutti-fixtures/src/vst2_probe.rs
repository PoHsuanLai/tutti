//! The reference VST2 plugin's exported probe symbols, as typed safe calls
//! (feature `vst2-probe`).
//!
//! As for the CLAP plugin ([`ClapProbe`](crate::ClapProbe)): the plugin's
//! switches and captures live in statics of its cdylib, which the host under
//! test loads, and the rlib linked for the `ProcessCapture` type is a separate
//! image with separate statics. [`Vst2Probe`] calls each `#[no_mangle]
//! extern "C"` export through a method with the export's exact signature. The
//! image is opened once and never closed, so what a test wrote survives.

use std::sync::OnceLock;

use libloading::Library;
use tutti_vst2_test_plugin::ProcessCapture;

/// The reference VST2 plugin, opened for its probe symbols.
///
/// Mutation (run): make `set_automate_burst` a no-op → the server's
/// `a_parameter_the_plugin_moved_is_forwarded` sees no burst → fails.
#[derive(Debug)]
pub struct Vst2Probe {
    image: Library,
}

impl Vst2Probe {
    /// The probe, opened on first use and resident for the rest of the
    /// process.
    ///
    /// # Panics
    ///
    /// If the cdylib has not been built or does not open.
    pub fn get() -> &'static Self {
        static PROBE: OnceLock<Vst2Probe> = OnceLock::new();
        PROBE.get_or_init(|| {
            #[allow(unsafe_code, reason = "opens the reference plugin; see the module")]
            // SAFETY: the path is the cdylib the dev-dependency built. Its
            // initialisers are the plugin's own, which only set statics.
            let image = unsafe { Library::new(crate::vst2_probe()) }.expect("re-open the probe");
            Self { image }
        })
    }

    /// What the plugin recorded of the host's calls (`valid` is false until it
    /// has rendered a block).
    pub fn capture(&self) -> ProcessCapture {
        let mut cap = ProcessCapture::empty();
        #[allow(unsafe_code, reason = "typed call into the reference plugin")]
        // SAFETY: `tutti_vst2_probe_capture` is `#[no_mangle] pub unsafe
        // extern "C" fn(out: *mut ProcessCapture) -> bool` in `capture.rs`,
        // and `cap` is a valid, exclusive `ProcessCapture` (`#[repr(C)]`).
        // Its reply is `cap.valid`, which the caller reads.
        unsafe {
            get::<unsafe extern "C" fn(*mut ProcessCapture) -> bool>(
                &self.image,
                b"tutti_vst2_probe_capture\0",
            )(&mut cap);
        }
        cap
    }

    /// Clears [`capture`](Self::capture).
    pub fn reset_capture(&self) {
        // SAFETY (of the name): `tutti_vst2_probe_reset_capture` is
        // `#[no_mangle] pub extern "C" fn()` in `capture.rs`.
        call_unit(&self.image, b"tutti_vst2_probe_reset_capture\0");
    }

    /// Puts every switch back to its default.
    pub fn reset_switches(&self) {
        // SAFETY (of the name): `tutti_vst2_probe_reset_switches` is
        // `#[no_mangle] pub extern "C" fn()` in `switches.rs`.
        call_unit(&self.image, b"tutti_vst2_probe_reset_switches\0");
    }

    /// Makes the plugin report `count` parameter changes of its own at its
    /// next render; 0 stops.
    pub fn set_automate_burst(&self, count: u32) {
        #[allow(unsafe_code, reason = "typed call into the reference plugin")]
        // SAFETY: `tutti_vst2_probe_set_automate_burst` is `#[no_mangle] pub
        // extern "C" fn(count: u32)` in `switches.rs`.
        unsafe {
            get::<extern "C" fn(u32)>(&self.image, b"tutti_vst2_probe_set_automate_burst\0")(count);
        }
    }

    /// The plugin raises `audioMasterIOChanged` with a new `latency` and
    /// `inputs` count; returns the host's answer.
    pub fn fire_io_changed(&self, latency: i32, inputs: i32) -> isize {
        #[allow(unsafe_code, reason = "typed call into the reference plugin")]
        // SAFETY: `tutti_vst2_probe_fire_io_changed` is `#[no_mangle] pub
        // extern "C" fn(latency: i32, inputs: i32) -> isize` in `switches.rs`.
        unsafe {
            get::<extern "C" fn(i32, i32) -> isize>(
                &self.image,
                b"tutti_vst2_probe_fire_io_changed\0",
            )(latency, inputs)
        }
    }
}

/// Resolves `name` in `lib` as an `F`.
///
/// # Safety
///
/// `name` must be an export of the reference plugin defined with exactly the
/// signature `F`. Every caller in this module names the definition it relies
/// on.
#[allow(unsafe_code, reason = "typed symbol lookup; see the module")]
unsafe fn get<F: Copy>(lib: &Library, name: &[u8]) -> F {
    // SAFETY: the caller's contract; the library is never closed, so the
    // pointer stays valid.
    let symbol: libloading::Symbol<F> = unsafe { lib.get(name) }.expect("probe export");
    *symbol
}

fn call_unit(lib: &Library, name: &[u8]) {
    #[allow(unsafe_code, reason = "typed call into the reference plugin")]
    // SAFETY: the two callers name `#[no_mangle] pub extern "C" fn()` exports
    // of the plugin (see them).
    unsafe {
        get::<extern "C" fn()>(lib, name)();
    }
}
