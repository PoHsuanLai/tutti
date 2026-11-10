//! In-process plugin editor.
//!
//! The plugin-server subprocess owns audio processing; the editor must live in
//! the host process because platform GUI toolkits are host-process-local. Each
//! format that is [`OutOfProcess`](tutti_plugin_types::OutOfProcess) loads an
//! editor-only second instance here through `load_editor` (design 021 §5); the
//! CLAP and VST3 ones live in their host crates. AU's is in this module until
//! its `Loaded` lands (design 021 step 7).

#[cfg(all(feature = "au", target_os = "macos"))]
mod au;

use std::path::Path;
use tutti_plugin_ipc::{BridgeError, Result};
use tutti_plugin_types::LoadStage;
use tutti_plugin_types::{Editor, MainThread};

#[cfg(all(any(feature = "vst3", feature = "clap", feature = "au"), unix))]
pub(crate) fn load_gui_instance(main: &MainThread, path: &Path) -> Result<Box<dyn Editor>> {
    use tutti_plugin_db::{PluginFormat, format_from_path};

    let format = format_from_path(path).ok_or_else(|| BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: LoadStage::Scanning,
        reason: format!("Unknown plugin format for: {}", path.display()),
    })?;

    match format {
        #[cfg(feature = "vst3")]
        PluginFormat::Vst3 => editor::<tutti_vst3_host::Vst3Loaded>(main, path, "VST3"),
        #[cfg(feature = "clap")]
        PluginFormat::Clap => editor::<tutti_clap_host::ClapLoaded>(main, path, "CLAP"),
        #[cfg(all(feature = "au", target_os = "macos"))]
        PluginFormat::AudioUnit => Ok(Box::new(au::AuGuiInstance::load(path)?)),
        PluginFormat::Vst2 => Err(BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: "VST2 plugins use the in-process backend (enable the \
                     `vst2` feature) — they don't go through the subprocess \
                     GUI loader."
                .into(),
        }),
        // `PluginFormat` is `#[non_exhaustive]` and another crate's, so the
        // arm stays even when every format above is compiled in (a `cfg` that
        // removed it then failed to build tutti-plugin with every format on
        // macOS).
        _ => Err(BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: format!("No in-process GUI support for format {format:?}"),
        }),
    }
}

/// `F`'s editor-only instance, boxed.
#[cfg(all(any(feature = "vst3", feature = "clap"), unix))]
fn editor<F: tutti_plugin_types::OutOfProcess>(
    main: &MainThread,
    path: &Path,
    format: &str,
) -> Result<Box<dyn Editor>>
where
    F::Editor: 'static,
{
    let editor = F::load_editor(main, path, &tutti_plugin_types::Load::new()).map_err(|e| {
        BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: format!("{format} GUI-only load failed: {e}"),
        }
    })?;
    Ok(Box::new(editor))
}

#[cfg(not(all(any(feature = "vst3", feature = "clap", feature = "au"), unix)))]
pub(crate) fn load_gui_instance(_main: &MainThread, path: &Path) -> Result<Box<dyn Editor>> {
    Err(BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: LoadStage::Opening,
        reason: "In-process GUI support not built (no gui features enabled)".into(),
    })
}
