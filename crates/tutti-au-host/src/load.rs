//! Finding an Audio Unit by the bundle it was installed from, and loading it.
//!
//! AU plugins are registered system-wide: the instance comes from the component
//! registry (`AudioComponentFindNext`), but the bundle says which units it
//! installed. The lookup reads the bundle's declared components and resolves
//! the chosen one by its exact type, subtype and manufacturer.

#![cfg(target_os = "macos")]

use std::path::Path;

use tutti_au_ffi::declared_components;
use tutti_plugin_types::{Load, LoadStage, MainThread, Origin, PluginError, PluginResult};

use crate::bus::BusDirection;
use crate::component::{AuType, find_component};
use crate::control::{descriptor_id, plugin_error};
use crate::instance::AuLoaded;
use crate::types::AudioComponentDescription;

/// The stream configuration a unit is created at: the activation applies its
/// own rate and block ceiling, so these only have to be a configuration the
/// unit takes.
const LOAD_RATE: f64 = 44_100.0;
const LOAD_BLOCK: u32 = 512;

/// The Audio Units the bundle at `path` declares, as the descriptor id a
/// [`Load`] names them by and the exact description the registry resolves.
fn declared(path: &Path) -> PluginResult<Vec<(String, AudioComponentDescription)>> {
    let scanning = |reason: String| PluginError::Load {
        stage: LoadStage::Scanning,
        reason,
    };
    let entries = declared_components(path).map_err(|e| {
        scanning(format!(
            "'{}' is not an Audio Unit bundle: {e}",
            path.display()
        ))
    })?;
    // The host classifies the types it knows; a declared type it cannot is no
    // unit it can load.
    let units: Vec<_> = entries
        .into_iter()
        .filter(|c| !matches!(AuType::from_raw(c.desc.componentType), AuType::Unknown(_)))
        .map(|c| (descriptor_id(&c.desc), c.desc))
        .collect();
    if units.is_empty() {
        return Err(scanning(format!(
            "'{}' declares no Audio Unit",
            path.display()
        )));
    }
    Ok(units)
}

/// The unit `load` selects among the bundle's `units`: the first with no
/// choice, otherwise the one whose id is the choice.
fn select(
    path: &Path,
    units: &[(String, AudioComponentDescription)],
    chosen: Option<&str>,
) -> PluginResult<AudioComponentDescription> {
    let Some(id) = chosen else {
        return Ok(units[0].1);
    };
    units
        .iter()
        .find(|(declared, _)| declared == id)
        .map(|(_, desc)| *desc)
        .ok_or_else(|| PluginError::Load {
            stage: LoadStage::Scanning,
            reason: format!(
                "'{}' declares no Audio Unit '{id}'; it declares: {}",
                path.display(),
                units
                    .iter()
                    .map(|(declared, _)| declared.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        })
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
    /// The unit is chosen from the bundle's own declaration (`AudioComponents`
    /// in its `Info.plist`) and resolved in the registry by its exact type,
    /// subtype and manufacturer. With no choice it is the bundle's first Audio
    /// Unit; a [`Load`] that names a plugin picks the one whose descriptor id
    /// (`au.<type>.<subtype>.<manufacturer>`, see
    /// [`PluginDescriptor::id`](tutti_plugin_types::PluginDescriptor::id)) it is.
    ///
    /// # Errors
    /// [`PluginError::Load`] with the stage reached: `Scanning` when the path
    /// is not a bundle that declares an Audio Unit, the choice is not among its
    /// units, or the chosen unit is not registered, `Instantiation` when the component cannot be
    /// created, `Initialization` for a bus no channel layout holds.
    pub fn load(main: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        let _ = main;
        let units = declared(path)?;
        let desc = select(path, &units, load.chosen())?;
        let component = find_component(&desc).ok_or_else(|| PluginError::Load {
            stage: LoadStage::Scanning,
            reason: format!(
                "'{}' declares {}, which the system has not registered",
                path.display(),
                descriptor_id(&desc)
            ),
        })?;
        let mut loaded = AuLoaded::new(component, LOAD_RATE, LOAD_BLOCK).map_err(|e| {
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
