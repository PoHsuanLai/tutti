//! **A probe reads a VST2 plugin's identity without activating it**: nothing is
//! resumed. (The server's `a_probe_does_not_activate`, moved with the probe.)
//!
//! The reference plugin records what the host did to it in the statics of its own
//! image, so the test re-opens the same file with `libloading` and reads the
//! capture, as the host's own suites do: the linked rlib is a separate image
//! with separate statics. The image is the plugin's copy under a `.vst` name, so
//! the probe routes it as VST2 whatever the platform's library extension.

#![cfg(feature = "vst2")]

use std::path::PathBuf;
use tutti_plugin_probe::describe;
use tutti_plugin_types::{Loaded, MainThread};
use tutti_vst2_host::Vst2Loaded;
use tutti_vst2_test_plugin::ProcessCapture;

fn plugin(dir: &tempfile::TempDir) -> PathBuf {
    let real = tutti_fixtures::cdylib("tutti_vst2_test_plugin", "tutti-vst2-test-plugin");
    let copy = dir.path().join("probe-target.vst");
    std::fs::copy(real, &copy).expect("copy the plugin");
    copy
}

/// One of the plugin's `extern "C"` exports in the image at `path`, which stays
/// mapped (the handle is leaked) so the host's image is the one read.
fn with_export<R>(path: &PathBuf, symbol: &str, f: impl FnOnce(*mut std::ffi::c_void) -> R) -> R {
    let name = format!("{symbol}\0");
    // SAFETY: the file is the cdylib the dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }.expect("re-open the plugin");
    // SAFETY: the symbol names are the plugin's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(name.as_bytes()) }.expect("plugin export");
    let r = f(*sym);
    std::mem::forget(lib);
    r
}

fn capture(path: &PathBuf) -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    with_export(path, "tutti_vst2_probe_capture", |p| {
        // SAFETY: the export has this signature.
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(p) };
        unsafe { f(&mut cap) };
    });
    cap
}

fn reset(path: &PathBuf) {
    with_export(path, "tutti_vst2_probe_reset_capture", |p| {
        // SAFETY: the export has this signature.
        let f: extern "C" fn() = unsafe { std::mem::transmute(p) };
        f();
    });
}

#[test]
fn a_probe_does_not_activate() {
    let main = MainThread::claim().expect("one test, one process");
    let dir = tempfile::tempdir().unwrap();
    let path = plugin(&dir);

    // Control: a real activation is seen.
    reset(&path);
    let loaded = Vst2Loaded::load(&main, &path).expect("loads");
    let prepare = tutti_plugin_types::Prepare::new(
        tutti_types::SampleRate(48_000.0),
        tutti_plugin_types::MaxBlock::new(tutti_types::Samples(64)).unwrap(),
    )
    .unwrap();
    let active = loaded.activate(&main, prepare);
    assert!(active.is_ok(), "the control activation");
    assert_eq!(
        capture(&path).resume_count,
        1,
        "the capture sees an activation"
    );
    drop(active);

    reset(&path);
    let descriptor = describe(&main, &path).expect("probes");
    assert_eq!(descriptor.name, "Tutti VST2 Probe");
    assert_eq!(capture(&path).resume_count, 0, "a probe resumed the plugin");
}
