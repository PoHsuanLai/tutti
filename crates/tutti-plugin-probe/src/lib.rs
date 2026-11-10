//! What `plugin-probe` does, as a library so a test can call it in-process: read
//! the descriptor of the plugin at a path, by the path's extension.
//!
//! The binary (`src/main.rs`) is the command line around [`describe`].

#![forbid(unsafe_code)]

use std::path::Path;
use tutti_plugin_types::{MainThread, PluginDescriptor};

/// The descriptor of the plugin at `path`, by its extension, or the reason it
/// could not be read. `main` proves the caller is on the main thread, which
/// VST2's load needs and the other formats' probes do not.
///
/// # Errors
///
/// The loader's own reason, as text: the file is missing, the format is not
/// compiled in, or the plugin would not open.
pub fn describe(main: &MainThread, path: &Path) -> Result<PluginDescriptor, String> {
    if !path.exists() {
        return Err(format!("Plugin not found: {}", path.display()));
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    // Only VST2 reads it; a build without that format has nothing to hand it to.
    let _ = main;
    match extension.as_str() {
        #[cfg(feature = "vst3")]
        "vst3" => vst3(path),
        #[cfg(feature = "clap")]
        "clap" => clap(path),
        #[cfg(feature = "vst2")]
        "vst" | "dll" | "so" => vst2(main, path),
        _ => Err(format!("Unsupported format: .{extension}")),
    }
}

/// The module a bundle directory holds for this target, or `path` itself when it
/// is a file (or names nothing, which the loader reports in its own words).
#[cfg(any(feature = "clap", feature = "vst3"))]
fn module_of(
    path: &Path,
    kind: tutti_plugin_types::bundle::ModuleKind,
) -> Result<std::path::PathBuf, String> {
    use tutti_plugin_types::bundle::{native_arch_subdirs, native_module_in_bundle};
    if path.is_file() || !path.is_dir() {
        return Ok(path.to_path_buf());
    }
    // `native_*`, not `any_*`: a module built for another CPU is a load failure
    // with extra steps, and "no module for this architecture" says more.
    native_module_in_bundle(path, kind).ok_or_else(|| {
        format!(
            "no plugin module in {} for this architecture (looked in Contents/{{{}}})",
            path.display(),
            native_arch_subdirs().join(", ")
        )
    })
}

#[cfg(feature = "clap")]
fn clap(path: &Path) -> Result<PluginDescriptor, String> {
    use tutti_clap_host::{ClapLoaded, descriptor_of};
    use tutti_plugin_types::EditorPresence;
    use tutti_plugin_types::bundle::ModuleKind;
    let module = module_of(path, ModuleKind::Clap)?;
    let library = (module != path).then_some(module.as_path());
    let info = ClapLoaded::probe(path, library).map_err(|e| e.to_string())?;
    // A probe does not instantiate, and an editor is a property of an instance.
    Ok(descriptor_of(&info, EditorPresence::Unknown))
}

#[cfg(feature = "vst3")]
fn vst3(path: &Path) -> Result<PluginDescriptor, String> {
    use tutti_plugin_types::EditorPresence;
    use tutti_plugin_types::bundle::ModuleKind;
    use tutti_vst3_host::{Vst3Loaded, descriptor_of};
    let module = module_of(path, ModuleKind::Vst3)?;
    let info = Vst3Loaded::probe(&module).map_err(|e| e.to_string())?;
    Ok(descriptor_of(&info, EditorPresence::Unknown))
}

/// VST2 has no probe short of a load: the plugin's init runs, it is not
/// activated, and the instance is dropped.
#[cfg(feature = "vst2")]
fn vst2(main: &MainThread, path: &Path) -> Result<PluginDescriptor, String> {
    use tutti_plugin_types::Control;
    use tutti_vst2_host::Vst2Loaded;
    let loaded = Vst2Loaded::load(main, path).map_err(|e| e.to_string())?;
    Ok(Control::info(&loaded).clone())
}
