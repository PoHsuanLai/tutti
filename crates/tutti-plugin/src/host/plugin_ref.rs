//! [`PluginRef`]: which plugin, in which file; and [`PluginState`]: a saved
//! state with the plugin it belongs to.
//!
//! A file can export several plugins (a CLAP or VST3 bundle, an AU component),
//! so a path alone does not name one. A [`PluginRef`] is the path, the format
//! and the plugin's own id inside it (its `PluginDescriptor::id`): what a
//! scan reports for each plugin, what the load opens, and what a project file
//! keeps to find the plugin again.

use std::path::{Path, PathBuf};
use tutti_plugin_db::{PluginEntry, PluginFormat, format_from_path};
use tutti_plugin_types::Load;

/// One plugin: the file it is in, its format, and its id in that file.
///
/// Made from a scanned plugin ([`PluginRef::from`] a
/// [`PluginEntry`]) or, for a file found by hand, [`file`](Self::file), which
/// names the file's first plugin. With the `serde` feature it persists, and
/// [`is_same_plugin`](Self::is_same_plugin) is how a reloaded project finds
/// out whether two refs name one plugin.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PluginRef {
    path: PathBuf,
    format: PluginFormat,
    /// The descriptor id; `None`: the file's first plugin.
    id: Option<String>,
}

impl PluginRef {
    /// The first plugin of the file at `path`, whose format is its extension's
    /// (`None` for an extension that is no plugin format).
    ///
    /// For a plugin known only by its file; one found by a scan has its own id
    /// (`PluginRef::from(&entry)`), which is what picks a plugin in a bundle
    /// that exports several.
    #[must_use]
    pub fn file(path: impl Into<PathBuf>) -> Option<Self> {
        let path = path.into();
        let format = format_from_path(&path)?;
        Some(Self {
            path,
            format,
            id: None,
        })
    }

    /// The file that holds the plugin.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The format whose loader opens it.
    #[must_use]
    pub fn format(&self) -> PluginFormat {
        self.format
    }

    /// The plugin's id in its file (its descriptor's `id`); `None` for the
    /// file's first plugin ([`file`](Self::file)).
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// The choice of plugin the loaders take: this plugin's id for a format
    /// whose files can hold several plugins (CLAP, VST3, AU), the file's first
    /// plugin when there is no id, and always for VST2, whose files hold one
    /// (and whose loader refuses a choice).
    #[must_use]
    pub fn load(&self) -> Load {
        match &self.id {
            Some(id) if self.format != PluginFormat::Vst2 => Load::new().plugin(id.clone()),
            _ => Load::new(),
        }
    }

    /// Whether `other` names the same plugin: the same format and, when both
    /// have an id, the same id; when either has none (it names the file's first
    /// plugin), the same file.
    #[must_use]
    pub fn is_same_plugin(&self, other: &Self) -> bool {
        self.format == other.format
            && match (&self.id, &other.id) {
                (Some(a), Some(b)) => a == b,
                _ => self.path == other.path,
            }
    }

    /// The plugin a load of `path` produced, whose descriptor reports
    /// `descriptor_id`; `None` for a path of no plugin format.
    pub(crate) fn loaded(path: &Path, descriptor_id: &str) -> Option<Self> {
        Some(Self {
            path: path.to_path_buf(),
            format: format_from_path(path)?,
            id: Some(descriptor_id.to_owned()),
        })
    }

    /// This ref with the id the loaded plugin reports, if it had none (an
    /// in-process load, which has no client to ask).
    #[cfg(feature = "vst2")]
    pub(crate) fn identified(&self, descriptor_id: &str) -> Self {
        Self {
            id: self.id.clone().or_else(|| Some(descriptor_id.to_owned())),
            ..self.clone()
        }
    }

    /// The plugin as text, for an error: its id, else its file.
    #[must_use]
    pub(crate) fn describe(&self) -> String {
        match &self.id {
            Some(id) => id.clone(),
            None => self.path.display().to_string(),
        }
    }
}

impl From<&PluginEntry<'_>> for PluginRef {
    fn from(entry: &PluginEntry<'_>) -> Self {
        Self {
            path: entry.path.to_path_buf(),
            format: entry.format,
            id: Some(entry.descriptor.id.clone()),
        }
    }
}

/// A plugin's saved state and the plugin it was saved from.
///
/// A state is the plugin's own opaque bytes. Restoring them into another
/// plugin is meaningless at best, so a state carries its [`PluginRef`] and a
/// restore into a plugin that [is not that one](PluginRef::is_same_plugin) is
/// refused with
/// [`StateError::OtherPlugin`](tutti_plugin_types::StateError::OtherPlugin).
/// A version of the plugin is not part of the identity: plugins read the
/// states of their older versions.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PluginState {
    plugin: PluginRef,
    bytes: Vec<u8>,
}

impl PluginState {
    /// `bytes`, the state `plugin` saved.
    #[must_use]
    pub fn new(plugin: PluginRef, bytes: Vec<u8>) -> Self {
        Self { plugin, bytes }
    }

    /// The plugin that saved it.
    #[must_use]
    pub fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

    /// The plugin's own bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The plugin's own bytes, by value.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Whether this state may be restored into `plugin`.
    ///
    /// # Errors
    ///
    /// [`StateError::OtherPlugin`](tutti_plugin_types::StateError::OtherPlugin)
    /// when it was saved from another plugin.
    pub fn check_for(&self, plugin: &PluginRef) -> Result<(), tutti_plugin_types::StateError> {
        if self.plugin.is_same_plugin(plugin) {
            Ok(())
        } else {
            Err(tutti_plugin_types::StateError::OtherPlugin {
                expected: plugin.describe(),
                found: self.plugin.describe(),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry_ref(path: &str, format: PluginFormat, id: Option<&str>) -> PluginRef {
        PluginRef {
            path: path.into(),
            format,
            id: id.map(Into::into),
        }
    }

    /// **The load follows the format.** A bundle format with an id chooses it;
    /// VST2 never does (its loader refuses a choice); no id is the first plugin.
    #[test]
    fn the_load_chooses_the_id_where_the_format_can_hold_several() {
        for format in [
            PluginFormat::Clap,
            PluginFormat::Vst3,
            PluginFormat::AudioUnit,
        ] {
            let r = entry_ref("/p/x", format, Some("the.id"));
            assert_eq!(r.load(), Load::new().plugin("the.id"), "{format:?}");
            assert_eq!(entry_ref("/p/x", format, None).load(), Load::new());
        }
        let vst2 = entry_ref("/p/x.vst", PluginFormat::Vst2, Some("vst2.1234"));
        assert_eq!(vst2.load(), Load::new());
    }

    /// **Identity is the id when both have one, the file otherwise.**
    #[test]
    fn two_refs_are_one_plugin_by_id_else_by_file() {
        let a = entry_ref("/p/bundle.clap", PluginFormat::Clap, Some("a"));
        let b = entry_ref("/p/bundle.clap", PluginFormat::Clap, Some("b"));
        let first = entry_ref("/p/bundle.clap", PluginFormat::Clap, None);
        let elsewhere = entry_ref("/q/bundle.clap", PluginFormat::Clap, None);
        assert!(a.is_same_plugin(&a));
        assert!(!a.is_same_plugin(&b), "one file, two plugins");
        assert!(a.is_same_plugin(&first), "the file's first plugin: by file");
        assert!(first.is_same_plugin(&a));
        assert!(!first.is_same_plugin(&elsewhere));
        let other_format = entry_ref("/p/bundle.clap", PluginFormat::Vst3, Some("a"));
        assert!(!a.is_same_plugin(&other_format), "the format is part of it");
    }

    /// **A file's ref has the extension's format, and refuses what is no plugin.**
    #[test]
    fn a_file_ref_takes_its_format_from_the_extension() {
        let r = PluginRef::file("/p/Synth.clap").unwrap();
        assert_eq!(r.format(), PluginFormat::Clap);
        assert_eq!(r.id(), None);
        assert!(PluginRef::file("/p/notes.txt").is_none());
    }

    /// **A state is refused by another plugin, and taken by its own.**
    #[test]
    fn a_state_checks_the_plugin_it_is_restored_into() {
        let a = entry_ref("/p/bundle.clap", PluginFormat::Clap, Some("a"));
        let b = entry_ref("/p/bundle.clap", PluginFormat::Clap, Some("b"));
        let state = PluginState::new(a.clone(), vec![1, 2, 3]);
        assert_eq!(state.check_for(&a), Ok(()));
        assert_eq!(
            state.check_for(&b),
            Err(tutti_plugin_types::StateError::OtherPlugin {
                expected: "b".into(),
                found: "a".into(),
            })
        );
    }
}
