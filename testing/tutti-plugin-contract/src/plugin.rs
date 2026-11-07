//! Where the shared plugin is, per format. It is one cdylib; a format whose
//! host dispatches on the extension gets a link to it under that extension.

use std::path::PathBuf;

/// The library, as cargo built it.
#[must_use]
pub fn library() -> PathBuf {
    tutti_fixtures::cdylib("tutti_shared_plugin", "tutti-shared-plugin")
}

/// The library published as `.clap`.
#[must_use]
pub fn clap() -> PathBuf {
    tutti_fixtures::publish_with_extension(&library(), "clap")
}

/// The library as the module of a `.vst3` bundle,
/// `<name>.vst3/Contents/<arch>/<module>`, the layout the VST3 host walks.
#[must_use]
pub fn vst3_module() -> PathBuf {
    let real = library();
    let dir = real
        .with_extension("vst3")
        .join("Contents")
        .join(tutti_plugin_types::bundle::native_arch_subdirs()[0]);
    std::fs::create_dir_all(&dir).expect("bundle directory");
    let module = dir.join(real.file_name().expect("file name"));
    #[cfg(unix)]
    if !std::fs::read_link(&module).is_ok_and(|to| to == real) {
        // Staged and renamed in, so a process loading it never sees it absent.
        let staging = dir.join(format!(".stage{}", std::process::id()));
        let _ = std::fs::remove_file(&staging);
        std::os::unix::fs::symlink(&real, &staging).expect("stage the bundle link");
        let _ = std::fs::rename(&staging, &module);
        let _ = std::fs::remove_file(&staging);
    }
    #[cfg(windows)]
    {
        // A copy, so a process loading it must never see it half written: stage
        // beside it and rename, and leave one that is already whole alone (a
        // loaded DLL cannot be replaced).
        let whole = |p: &std::path::Path| {
            std::fs::metadata(p)
                .and_then(|m| Ok(m.len() == std::fs::metadata(&real)?.len()))
                .unwrap_or(false)
        };
        if !whole(&module) {
            let staging = dir.join(format!(".stage{}", std::process::id()));
            std::fs::copy(&real, &staging).expect("stage the bundle copy");
            let _ = std::fs::rename(&staging, &module);
            let _ = std::fs::remove_file(&staging);
        }
    }
    assert!(module.exists(), "no module at {}", module.display());
    module
}

/// The C++ plugin's module, in the bundle its crate's `build.rs` links.
#[must_use]
pub fn vst3_cpp_module() -> PathBuf {
    let dir = vst3_cpp_bundle()
        .join("Contents")
        .join(tutti_plugin_types::bundle::native_arch_subdirs()[0]);
    std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("the C++ plugin bundle {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.is_file())
        .unwrap_or_else(|| panic!("no module in {}", dir.display()))
}

/// The `.vst3` directory around a module. The plugin-server picks the format
/// from a path's extension, so it takes this, not the module: a module path
/// ending in `.so` is loaded as VST2.
///
/// # Panics
///
/// When `module` is not inside a `.vst3` bundle.
#[must_use]
pub fn bundle_of(module: &std::path::Path) -> PathBuf {
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
