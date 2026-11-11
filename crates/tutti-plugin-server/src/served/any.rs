//! The `served!` table: which formats this server runs, as one enum.
//!
//! A row is a variant, the format's [`Loaded`](tutti_plugin_types::Loaded)
//! type, and the extension its files carry. The row's type is the argument of
//! [`Served`], which is bounded by
//! [`OutOfProcess`](tutti_plugin_types::OutOfProcess) (through
//! [`ServerSide`](super::ServerSide)), so a row for a format that cannot be
//! served from another process (VST2: its `AEffect` fuses editor and audio,
//! design 021 §4) does not compile. Adding a format is one row and, if it
//! differs from the defaults in what this process does, a `ServerSide` impl.

use std::path::Path;

use tutti_plugin_types::{Load, LoadStage, MainThread, PluginError, PluginResult};

use super::{Served, ServedPlugin};

macro_rules! served {
    ($($(#[$cfg:meta])* $name:ident($loaded:ty, $extension:literal)),* $(,)?) => {
        /// One loaded plugin, whichever format backs it.
        ///
        /// Every variant is behind its format's feature (AU also behind macOS),
        /// so a build with none has an uninhabited enum.
        #[allow(clippy::large_enum_variant)] // One per subprocess; boxing buys nothing.
        pub(crate) enum AnyServed {
            $($(#[$cfg])* $name(Served<$loaded>),)*
        }

        impl AnyServed {
            /// Loads the plugin `load` chooses in the file at `path` and
            /// activates it, choosing the format from the file's extension.
            ///
            /// # Errors
            ///
            /// [`PluginError::Load`] at [`LoadStage::Scanning`] when the path
            /// does not exist, at [`LoadStage::Opening`] for an extension no
            /// row names, else the format's own.
            #[allow(clippy::too_many_arguments)] // one load's parameters, as the wire carries them
            pub(crate) fn load(
                main: &MainThread,
                path: &Path,
                load: &Load,
                sample_rate: f64,
                block_size: usize,
                prefer_f64: bool,
                state: Option<&[u8]>,
                before_restore: &mut dyn FnMut(),
            ) -> PluginResult<(Self, Option<String>)> {
                if !path.exists() {
                    return Err(PluginError::Load {
                        stage: LoadStage::Scanning,
                        reason: "Plugin not found".to_string(),
                    });
                }
                let extension = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                match extension.as_str() {
                    $($(#[$cfg])* $extension => {
                        let (plugin, state_error) = Served::<$loaded>::load(
                            main,
                            path,
                            load,
                            sample_rate,
                            block_size,
                            prefer_f64,
                            state,
                            before_restore,
                        )?;
                        Ok((Self::$name(plugin), state_error))
                    })*
                    _ => {
                        let served: &[&str] = &[$($(#[$cfg])* concat!(".", $extension)),*];
                        Err(PluginError::Load {
                            stage: LoadStage::Opening,
                            reason: format!(
                                "Unsupported plugin format: {extension}. Supported: {}.",
                                served.join(", ")
                            ),
                        })
                    }
                }
            }

            /// The plugin, its format erased.
            #[allow(unreachable_patterns)] // every arm is feature-gated: with none, no value exists
            pub(crate) fn plugin(&self) -> &dyn ServedPlugin {
                match self {
                    $($(#[$cfg])* Self::$name(plugin) => plugin,)*
                    _ => unreachable!("no format is served"),
                }
            }

            /// [`plugin`](Self::plugin), exclusively.
            #[allow(unreachable_patterns)] // as above
            pub(crate) fn plugin_mut(&mut self) -> &mut dyn ServedPlugin {
                match self {
                    $($(#[$cfg])* Self::$name(plugin) => plugin,)*
                    _ => unreachable!("no format is served"),
                }
            }
        }
    };
}

served! {
    #[cfg(feature = "clap")]
    Clap(tutti_clap_host::ClapLoaded, "clap"),
    #[cfg(feature = "vst3")]
    Vst3(tutti_vst3_host::Vst3Loaded, "vst3"),
    #[cfg(all(feature = "au", target_os = "macos"))]
    Au(tutti_au_host::AuLoaded, "component"),
}
