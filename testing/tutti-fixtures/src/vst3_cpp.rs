//! Where the C++ twin of the shared plugin is (feature `vst3-cpp`).

use std::path::{Path, PathBuf};

/// The C++ plugin's module, in the bundle its crate's `build.rs` links.
#[must_use]
pub fn vst3_cpp_module() -> PathBuf {
    // The extension its `build.rs` gives the module.
    let ext = if cfg!(windows) {
        "vst3"
    } else if cfg!(target_os = "macos") {
        "dylib"
    } else {
        "so"
    };
    let module = vst3_cpp_bundle()
        .join("Contents")
        .join(tutti_plugin_types::bundle::native_arch_subdirs()[0])
        .join(format!("shared-vst3-cpp.{ext}"));
    assert!(
        module.is_file(),
        "no C++ plugin module at {}",
        module.display()
    );
    module
}

/// The `.vst3` directory around a module. The plugin-server picks the format
/// from a path's extension, so it takes this, not the module: a module path
/// ending in `.so` is loaded as VST2.
///
/// # Panics
///
/// When `module` is not inside a `.vst3` bundle.
#[must_use]
pub fn bundle_of(module: &Path) -> PathBuf {
    module
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "vst3"))
        .expect("a .vst3 bundle around the module")
        .to_path_buf()
}

/// The C++ plugin's bundle directory.
#[must_use]
pub fn vst3_cpp_bundle() -> PathBuf {
    PathBuf::from(env!("SHARED_VST3_CPP_DIR")).join("shared-vst3-cpp.vst3")
}
