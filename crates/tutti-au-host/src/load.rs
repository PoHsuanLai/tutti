//! Finding an Audio Unit by the bundle it was installed from, and loading it.
//!
//! AU plugins are registered system-wide: the path names the bundle, but the
//! instance comes from the component registry (`AudioComponentFindNext`), so
//! the lookup matches the bundle's file stem against the registered names.

#![cfg(target_os = "macos")]

use std::path::Path;

use tutti_plugin_types::{Load, LoadStage, MainThread, Origin, PluginError, PluginResult};

use crate::component::{AuComponentInfo, enumerate_components};
use crate::control::plugin_error;
use crate::instance::AuLoaded;
use crate::bus::BusDirection;

/// The stream configuration a unit is created at: the activation applies its
/// own rate and block ceiling, so these only have to be a configuration the
/// unit takes.
const LOAD_RATE: f64 = 44_100.0;
const LOAD_BLOCK: u32 = 512;

/// The registered component whose name matches the bundle's file stem.
fn find(path: &Path) -> PluginResult<AuComponentInfo> {
    let bundle_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_string();

    let components = enumerate_components();
    let matching = components.iter().find(|c| {
        // AU names are typically "Manufacturer: PluginName": match if the name
        // contains this plugin's bundle name, or is exactly the part after ": ".
        c.name.contains(&bundle_name)
            || c.name.ends_with(&bundle_name)
            || c.name.split(": ").last().is_some_and(|n| n == bundle_name)
    });
    match matching {
        Some(info) => Ok(info.clone()),
        None => Err(PluginError::Load {
            stage: LoadStage::Scanning,
            reason: format!(
                "No Audio Unit component found matching '{}'. Available AUs: {}",
                bundle_name,
                components
                    .iter()
                    .take(10)
                    .map(|c| c.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
    }
}

impl AuLoaded {
    /// Loads the Audio Unit installed from the bundle at `path`, not initialized.
    ///
    /// The host transport callbacks are offered once, here, off the audio
    /// thread: the AU pulls tempo, beat and transport state from them during its
    /// render. A unit that refuses the property simply runs on its own clock
    /// ([`transport`](Self::transport) is then `None`, and its features say so),
    /// not a failed load. The property listener is started
    /// ([`watch_properties`](Self::watch_properties)).
    ///
    /// An AU file holds one plugin, so a [`Load`] that names one is refused.
    ///
    /// # Errors
    /// [`PluginError::Load`] with the stage reached: `Scanning` when no
    /// registered component matches, `Instantiation` when the component cannot be
    /// created, `Initialization` for a bus no channel layout holds.
    pub fn load(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        let _ = main;
        if let Some(chosen) = load.chosen() {
            return Err(PluginError::Load {
                stage: LoadStage::Opening,
                reason: format!("an Audio Unit bundle holds one plugin, not '{chosen}'"),
            });
        }
        let info = find(path)?;
        let mut loaded = AuLoaded::new(info.component, LOAD_RATE, LOAD_BLOCK).map_err(|e| {
            match plugin_error(e) {
                // The component's own failure, at the stage it reports.
                load @ PluginError::Load { .. } => load,
                other => PluginError::Load {
                    stage: LoadStage::Instantiation,
                    reason: other.to_string(),
                },
            }
        })?;
        // A bus no layout holds fails the load, naming the side.
        for direction in [BusDirection::Input, BusDirection::Output] {
            loaded.buses(direction).map_err(|e| PluginError::Load {
                stage: LoadStage::Initialization,
                reason: e.to_string(),
            })?;
        }
        let _ = loaded.install_host_callbacks();
        let _ = loaded.watch_properties();
        loaded.origin = Some(Origin::new(path, load.clone()));
        Ok(loaded)
    }

    /// Where the unit was loaded from, when it came through [`load`](Self::load).
    #[must_use]
    pub fn origin(&self) -> Option<&Origin> {
        self.origin.as_ref()
    }
}
