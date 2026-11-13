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
    let module = dir.join(real.file_name().expect("file name"));
    // Every case process stages the same link, and macOS CI has seen one find
    // no module right after staging it ("no module at"). Each attempt stages
    // again, and the panic says what the bundle held, so a recurrence shows its
    // cause instead of only its symptom.
    for _ in 0..STAGE_ATTEMPTS {
        stage_module(&real, &dir, &module);
        if module.exists() {
            return module;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    panic!(
        "no module at {} after {STAGE_ATTEMPTS} attempts: the link reads {:?}, \
         the library {} exists: {}, the bundle directory holds {:?}",
        module.display(),
        std::fs::read_link(&module),
        real.display(),
        real.exists(),
        std::fs::read_dir(&dir).map(|d| d.flatten().map(|e| e.file_name()).collect::<Vec<_>>()),
    );
}

/// How many times [`vst3_module`] stages the bundle's module before it fails.
const STAGE_ATTEMPTS: usize = 5;

/// Puts `real` at `module` inside `dir`, unless it is there already. Staged
/// under a name of this process's own and renamed in, so a process loading it
/// never sees it half made.
fn stage_module(real: &std::path::Path, dir: &std::path::Path, module: &std::path::Path) {
    std::fs::create_dir_all(dir).expect("bundle directory");
    #[cfg(unix)]
    if !std::fs::read_link(module).is_ok_and(|to| to == real) {
        let staging = dir.join(format!(".stage{}", std::process::id()));
        let _ = std::fs::remove_file(&staging);
        std::os::unix::fs::symlink(real, &staging).expect("stage the bundle link");
        let _ = std::fs::rename(&staging, module);
        let _ = std::fs::remove_file(&staging);
    }
    #[cfg(windows)]
    {
        // A copy, so a process loading it must never see it half written: stage
        // beside it and rename, and leave one that is already whole alone (a
        // loaded DLL cannot be replaced).
        let whole = |p: &std::path::Path| {
            std::fs::metadata(p)
                .and_then(|m| Ok(m.len() == std::fs::metadata(real)?.len()))
                .unwrap_or(false)
        };
        if !whole(module) {
            let staging = dir.join(format!(".stage{}", std::process::id()));
            std::fs::copy(real, &staging).expect("stage the bundle copy");
            let _ = std::fs::rename(&staging, module);
            let _ = std::fs::remove_file(&staging);
        }
    }
}

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

/// The shared plugin as an Audio Unit: registers it in this process (once) and
/// returns the `.component` directory a host is given to find it by.
///
/// An AU is found in the system's component registry, and the bundle only says
/// which units it installed (`AudioComponents` in its `Info.plist`). The shared
/// plugin's AU is registered in-process (`tutti_shared_plugin::register_au`), so
/// the bundle is just that declaration: no executable, nothing installed. The
/// host's own lookup, bundle then exact description in the registry, is the one
/// an installed unit takes.
///
/// # Panics
///
/// When the bundle cannot be written.
#[cfg(target_os = "macos")]
#[must_use]
pub fn au_bundle() -> PathBuf {
    use tutti_shared_plugin::{AU_MANUFACTURER, AU_NAME, AU_SUBTYPE, AU_TYPE, AU_VERSION};
    tutti_shared_plugin::register_au();
    let code = |c: [u8; 4]| String::from_utf8(c.to_vec()).expect("an ASCII code");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>dev.tutti.shared-plugin-au</string>
  <key>CFBundlePackageType</key><string>BNDL</string>
  <key>CFBundleName</key><string>tutti-shared-plugin</string>
  <key>AudioComponents</key>
  <array>
    <dict>
      <key>type</key><string>{}</string>
      <key>subtype</key><string>{}</string>
      <key>manufacturer</key><string>{}</string>
      <key>name</key><string>{AU_NAME}</string>
      <key>version</key><integer>{AU_VERSION}</integer>
    </dict>
  </array>
</dict>
</plist>
"#,
        code(AU_TYPE),
        code(AU_SUBTYPE),
        code(AU_MANUFACTURER),
    );
    let bundle = library().with_extension("component");
    let contents = bundle.join("Contents");
    std::fs::create_dir_all(&contents).expect("bundle directory");
    // Staged under this process's own name and renamed in, so a process that
    // reads it never sees it half written.
    let staging = contents.join(format!(".stage{}", std::process::id()));
    std::fs::write(&staging, plist).expect("stage the Info.plist");
    std::fs::rename(&staging, contents.join("Info.plist")).expect("place the Info.plist");
    bundle
}
