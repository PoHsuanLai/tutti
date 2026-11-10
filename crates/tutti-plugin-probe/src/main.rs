//! `plugin-probe <path> <result-file>`: load a plugin just far enough to read its
//! descriptor, write the outcome to the result file and exit.
//!
//! The result file is the only wire (`tutti_plugin_types::write_result`: a
//! staging file renamed into place). Plugins print from their init code, so
//! this process's stdout and stderr carry nothing; the host that spawns it
//! points them at the null device. A file at `<result-file>` means the probe
//! finished, with a descriptor or a typed failure; no file means it crashed or
//! hung, which the host tells from the process.
//!
//! Exit status: `0` once a result is written (a plugin that would not load is a
//! result, not an error), `2` for a bad command line, `3` if the result could
//! not be written.
//!
//! Formats are cargo features (`clap`, `vst3`, `vst2`); a probe built without
//! the one a path names reports that as a failure. Audio Units join when their
//! host implements `Loaded` (design 021, step 4c).

#![forbid(unsafe_code)]

use std::path::Path;
use std::process::ExitCode;
use tutti_plugin_types::{PluginDescriptor, ProbeOutcome, write_result};

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let (Some(path), Some(result), None) = (args.next(), args.next(), args.next()) else {
        eprintln!("usage: plugin-probe <plugin-path> <result-file>");
        return ExitCode::from(2);
    };
    let outcome = match describe(Path::new(&path)) {
        Ok(descriptor) => ProbeOutcome::Described(descriptor),
        Err(message) => ProbeOutcome::Failed(message),
    };
    match write_result(Path::new(&result), &outcome) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!(
                "plugin-probe: cannot write {}: {e}",
                Path::new(&result).display()
            );
            ExitCode::from(3)
        }
    }
}

/// The descriptor of the plugin at `path`, by its extension, or the reason it
/// could not be read.
fn describe(path: &Path) -> Result<PluginDescriptor, String> {
    if !path.exists() {
        return Err(format!("Plugin not found: {}", path.display()));
    }
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    match extension.as_str() {
        #[cfg(feature = "vst3")]
        "vst3" => vst3(path),
        #[cfg(feature = "clap")]
        "clap" => clap(path),
        #[cfg(feature = "vst2")]
        "vst" | "dll" | "so" => vst2(path),
        _ => Err(format!("Unsupported format: .{extension}")),
    }
}

/// The module a bundle directory holds for this target, or `path` itself when it
/// is a file (or names nothing, which the loader reports in its own words).
#[cfg(any(feature = "clap", feature = "vst3"))]
fn module_of(path: &Path) -> Result<std::path::PathBuf, String> {
    use tutti_plugin_types::bundle::{ModuleKind, native_arch_subdirs, native_module_in_bundle};
    if path.is_file() || !path.is_dir() {
        return Ok(path.to_path_buf());
    }
    // `native_*`, not `any_*`: a module built for another CPU is a load failure
    // with extra steps, and "no module for this architecture" says more.
    native_module_in_bundle(path, ModuleKind::Vst3).ok_or_else(|| {
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
    let module = module_of(path)?;
    let library = (module != path).then_some(module.as_path());
    let info = ClapLoaded::probe(path, library).map_err(|e| e.to_string())?;
    // A probe does not instantiate, and an editor is a property of an instance.
    Ok(descriptor_of(&info, EditorPresence::Unknown))
}

#[cfg(feature = "vst3")]
fn vst3(path: &Path) -> Result<PluginDescriptor, String> {
    use tutti_plugin_types::EditorPresence;
    use tutti_vst3_host::{Vst3Loaded, descriptor_of};
    let module = module_of(path)?;
    let info = Vst3Loaded::probe(&module).map_err(|e| e.to_string())?;
    Ok(descriptor_of(&info, EditorPresence::Unknown))
}

/// VST2 has no probe short of a load: the plugin's init runs, it is not
/// activated, and the instance is dropped.
#[cfg(feature = "vst2")]
fn vst2(path: &Path) -> Result<PluginDescriptor, String> {
    use tutti_plugin_types::{Control, MainThread};
    use tutti_vst2_host::Vst2Loaded;
    // This process runs one probe on one thread, which is its main thread.
    let main = MainThread::claim().map_err(|e| e.to_string())?;
    let loaded = Vst2Loaded::load(&main, path).map_err(|e| e.to_string())?;
    Ok(Control::info(&loaded).clone())
}
