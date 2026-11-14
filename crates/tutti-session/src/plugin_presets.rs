//! A hosted plugin's presets: what the format can do
//! with them, the list where there is one, and a load that says what happened.
//!
//! Presets are the main thread's (VST3, VST2 and AU answer them there), so the
//! view is [`MainPlugin::presets`](crate::MainPlugin::presets), and the read-only
//! [`Plugin`](crate::Plugin) has none. The three answers of the old
//! `Option<bool>` are [`PresetError`]: no such operation for this plugin
//! ([`Unsupported`](PresetError::Unsupported)), the plugin declined
//! ([`Refused`](PresetError::Refused)), the plugin is gone
//! ([`Lost`](PresetError::Lost)).

use std::path::Path;

use crate::plugin::HostedPlugin;
use tutti_plugin_types::{MainThreadMarker, Preset, PresetError, PresetId, PresetSupport};

/// What a plugin's format does with presets ([`Presets::kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PresetKind {
    /// The plugin lists its presets and loads one by its id (VST2, VST3, AU):
    /// [`Presets::iter`], [`PresetEntry::load`].
    Browse,
    /// The plugin loads a preset from a file and lists none (CLAP, whose
    /// discovery is not bound): [`Presets::load_file`].
    File,
}

/// A hosted plugin's presets, lent by [`MainPlugin::presets`](crate::MainPlugin::presets).
///
/// The list is read when the view is made, so a call may be a round trip to a
/// served plugin; the view is a snapshot of it, and the plugin's own list is
/// asked again by making another.
#[derive(Debug)]
pub struct Presets<'a> {
    hosted: &'a HostedPlugin,
    main: MainThreadMarker,
    kind: PresetKind,
    list: Vec<Preset>,
}

impl<'a> Presets<'a> {
    /// The presets of the plugin of `hosted`.
    ///
    /// # Errors
    ///
    /// [`PresetError::Unsupported`]: the plugin has no preset surface;
    /// [`PresetError::Lost`]: it is gone.
    pub(crate) fn of(
        hosted: &'a HostedPlugin,
        main: MainThreadMarker,
    ) -> Result<Self, PresetError> {
        let handle = &hosted.handle;
        let kind = match handle.preset_support() {
            // A format that lists but cannot load answers its loads `Refused`.
            PresetSupport::Full | PresetSupport::ListOnly => PresetKind::Browse,
            PresetSupport::LoadByPath => PresetKind::File,
            PresetSupport::None => return Err(PresetError::Unsupported),
        };
        let list = match kind {
            PresetKind::Browse => handle.presets(main)?,
            PresetKind::File => Vec::new(),
        };
        Ok(Self {
            hosted,
            main,
            kind,
            list,
        })
    }

    /// Whether the plugin lists its presets or loads them from files.
    pub fn kind(&self) -> PresetKind {
        self.kind
    }

    /// How many presets the plugin listed (always 0 for [`PresetKind::File`]).
    pub fn len(&self) -> usize {
        self.list.len()
    }

    /// Whether it listed none.
    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// The listed presets, in the plugin's own order.
    pub fn iter(&self) -> impl Iterator<Item = PresetEntry<'_>> {
        self.list.iter().map(|preset| PresetEntry {
            presets: self,
            preset,
        })
    }

    /// The listed preset with `id`, if the plugin listed one.
    pub fn get(&self, id: &PresetId) -> Option<PresetEntry<'_>> {
        self.iter().find(|entry| entry.id() == id)
    }

    /// The preset the plugin says is loaded, when it will say: not every format
    /// has the query, and the plugin may decline. Never "the first one".
    pub fn current(&self) -> Option<PresetId> {
        self.hosted.handle.current_preset(self.main)
    }

    /// Loads the preset file at `path` ([`PresetKind::File`]).
    ///
    /// # Errors
    ///
    /// [`PresetError::Unsupported`] for a plugin that lists its presets and loads
    /// no file; [`PresetError::Refused`] when the plugin cannot read the file;
    /// [`PresetError::Lost`] when it is gone.
    pub fn load_file(&self, path: impl AsRef<Path>) -> Result<(), PresetError> {
        if self.kind != PresetKind::File {
            return Err(PresetError::Unsupported);
        }
        self.hosted
            .handle
            .load_preset(self.main, &PresetId::Location(path.as_ref().to_path_buf()))?;
        self.hosted.refresh_values();
        Ok(())
    }
}

/// One listed preset ([`Presets::iter`]).
#[derive(Debug, Clone, Copy)]
pub struct PresetEntry<'a> {
    presets: &'a Presets<'a>,
    preset: &'a Preset,
}

impl PresetEntry<'_> {
    /// The plugin's id for it: opaque, never a position in the list.
    pub fn id(&self) -> &PresetId {
        &self.preset.id
    }

    /// Its name.
    pub fn name(&self) -> &str {
        &self.preset.name
    }

    /// The bank it is in, for a format that groups.
    pub fn bank(&self) -> Option<&str> {
        self.preset.bank.as_deref()
    }

    /// Loads it into the plugin.
    ///
    /// # Errors
    ///
    /// [`PresetError::Refused`] when the plugin declines (leave the selection
    /// where it was); [`PresetError::Lost`] when it is gone.
    pub fn load(&self) -> Result<(), PresetError> {
        let presets = self.presets;
        presets
            .hosted
            .handle
            .load_preset(presets.main, &self.preset.id)?;
        presets.hosted.refresh_values();
        Ok(())
    }
}
