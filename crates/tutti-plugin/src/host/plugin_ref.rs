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
use tutti_plugin_types::{BridgeError, BridgeResult, Load};

/// Where a plugin runs: in the host's own process or in a `plugin-server`
/// process. A choice made per plugin, as a [`PluginRef`] field
/// ([`PluginRef::with_hosting`]); everything a loaded plugin offers is the same
/// either way.
///
/// The default, [`Auto`](Self::Auto), is the format's own:
/// CLAP, VST3 and AU run [out of process](Self::OutOfProcess), which isolates a
/// crash and keeps the plugin out of the host's address space; VST2 runs [in
/// process](Self::InProcess), the only way it can run, since its `AEffect`
/// fuses the editor and the audio instance. `Auto` never moves a plugin the
/// catalog has blacklisted into the host's process: a blacklisted
/// plugin is refused at the load, whatever the hosting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Hosting {
    /// The format's default: out of process, except VST2, which is in process.
    #[default]
    Auto,
    /// In the host's process: no isolation, no IPC copy, and the editor is the
    /// audio instance. The load needs the main thread
    /// ([`MainThread`](tutti_plugin_types::MainThread)). Refused with
    /// [`BridgeError::NotInProcess`]
    /// when this build has no host for the format (its `tutti-plugin` feature
    /// is off).
    InProcess,
    /// In a `plugin-server` process, with its editor here. Refused with
    /// [`BridgeError::NotOutOfProcess`]
    /// for VST2, which cannot be served.
    OutOfProcess,
}

impl Hosting {
    /// Where a load of the `format` plugin at `path` runs: `InProcess` or
    /// `OutOfProcess`, never `Auto`.
    ///
    /// # Errors
    ///
    /// [`BridgeError::NotOutOfProcess`] for a VST2 plugin asked to be served;
    /// [`BridgeError::NotInProcess`] for a format this build has no in-process
    /// host for (VST2 included, with its `vst2` feature off).
    pub(crate) fn resolve(self, path: &Path, format: PluginFormat) -> BridgeResult<Self> {
        let name = format_name(format);
        match (self, format) {
            (Self::OutOfProcess, PluginFormat::Vst2) => Err(BridgeError::NotOutOfProcess {
                path: path.to_path_buf(),
                format: name,
            }),
            (Self::OutOfProcess, _) => Ok(Self::OutOfProcess),
            (Self::InProcess, _) | (Self::Auto, PluginFormat::Vst2) => {
                if in_process_built(format) {
                    Ok(Self::InProcess)
                } else {
                    Err(BridgeError::NotInProcess {
                        path: path.to_path_buf(),
                        format: name,
                    })
                }
            }
            (Self::Auto, _) => Ok(Self::OutOfProcess),
        }
    }
}

/// The format as a user knows it.
fn format_name(format: PluginFormat) -> &'static str {
    match format {
        PluginFormat::Vst3 => "VST3",
        PluginFormat::Vst2 => "VST2",
        PluginFormat::Clap => "CLAP",
        PluginFormat::AudioUnit => "AU",
        _ => "plugin",
    }
}

/// Whether this build holds the format's host for loading in this process.
fn in_process_built(format: PluginFormat) -> bool {
    let built = [
        (PluginFormat::Vst2, cfg!(feature = "vst2")),
        (PluginFormat::Clap, cfg!(feature = "clap")),
        (PluginFormat::Vst3, cfg!(feature = "vst3")),
        (
            PluginFormat::AudioUnit,
            cfg!(all(feature = "au", target_os = "macos")),
        ),
    ];
    built.iter().any(|&(has, on)| has == format && on)
}

/// One plugin: the file it is in, its format, and its id in that file.
///
/// Made from a scanned plugin ([`PluginRef::from`] a
/// [`PluginEntry`]) or, for a file found by hand, [`file`](Self::file), which
/// names the file's first plugin. With the `serde` feature it persists, and
/// `is_same_plugin` is how a reloaded project finds
/// out whether two refs name one plugin.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PluginRef {
    path: PathBuf,
    format: PluginFormat,
    /// The descriptor id; `None`: the file's first plugin.
    id: Option<String>,
    /// Where it runs. Not part of which plugin it is
    /// ([`is_same_plugin`](Self::is_same_plugin)).
    #[cfg_attr(feature = "serde", serde(default))]
    hosting: Hosting,
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
            hosting: Hosting::Auto,
        })
    }

    /// This plugin, hosted as `hosting` ([`Hosting::Auto`] is what a ref starts
    /// with). Whether the format can be hosted that way is decided at the load.
    #[must_use]
    pub fn with_hosting(mut self, hosting: Hosting) -> Self {
        self.hosting = hosting;
        self
    }

    /// Where the plugin is to run, as asked: [`Hosting::Auto`] until
    /// [`with_hosting`](Self::with_hosting) says otherwise.
    #[must_use]
    pub fn hosting(&self) -> Hosting {
        self.hosting
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

    /// The choice of plugin the loaders take: this plugin's id (a CLAP plugin
    /// id, a VST3 class id, an AU unit, a VST2 shell's `vst2.<uniqueID>`), or
    /// the file's first plugin when there is no id.
    #[must_use]
    pub fn load(&self) -> Load {
        match &self.id {
            Some(id) => Load::new().plugin(id.clone()),
            None => Load::new(),
        }
    }

    /// Whether `other` names the same plugin: the same format and, when both
    /// have an id, the same id; when either has none (it names the file's first
    /// plugin), the same file.
    #[must_use]
    pub(crate) fn is_same_plugin(&self, other: &Self) -> bool {
        self.format == other.format
            && match (&self.id, &other.id) {
                (Some(a), Some(b)) => a == b,
                // Canonical, as a scan keys a file: a link and the file it
                // names are one plugin.
                _ => {
                    tutti_plugin_db::canonical_path(&self.path)
                        == tutti_plugin_db::canonical_path(&other.path)
                }
            }
    }

    /// The plugin a load of `path` produced, whose descriptor reports
    /// `descriptor_id`; `None` for a path of no plugin format.
    pub(crate) fn loaded(path: &Path, descriptor_id: &str) -> Option<Self> {
        Some(Self {
            path: path.to_path_buf(),
            format: format_from_path(path)?,
            id: Some(descriptor_id.to_owned()),
            hosting: Hosting::Auto,
        })
    }

    /// This ref with the id the loaded plugin reports, if it had none (an
    /// in-process load, which has no client to ask).
    #[cfg(any(
        feature = "vst2",
        feature = "clap",
        feature = "vst3",
        all(feature = "au", target_os = "macos")
    ))]
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
            hosting: Hosting::Auto,
        }
    }
}

/// A plugin's saved state and the plugin it was saved from.
///
/// A state is the plugin's own opaque bytes. Restoring them into another
/// plugin is meaningless at best, so a state carries its [`PluginRef`] and a
/// restore into a plugin that is not that one (`PluginRef::is_same_plugin`) is
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
            hosting: Hosting::Auto,
        }
    }

    /// **`Auto` is the format's default**: CLAP, VST3 and AU
    /// are served, whatever the build holds.
    #[test]
    fn auto_serves_every_format_but_vst2() {
        for format in [
            PluginFormat::Clap,
            PluginFormat::Vst3,
            PluginFormat::AudioUnit,
        ] {
            let path = Path::new("P");
            assert_eq!(
                Hosting::Auto.resolve(path, format).unwrap(),
                Hosting::OutOfProcess,
                "{format:?}"
            );
        }
    }

    /// **VST2 cannot be served, and is not quietly hosted here instead.**
    #[test]
    fn vst2_asked_for_out_of_process_is_refused() {
        let err = Hosting::OutOfProcess
            .resolve(Path::new("P.vst"), PluginFormat::Vst2)
            .unwrap_err();
        assert!(matches!(
            err,
            BridgeError::NotOutOfProcess { format: "VST2", .. }
        ));
    }

    /// **A hosting the build has no host for is refused, typed**: the format's
    /// feature decides, for `Auto` on VST2 as for an explicit `InProcess`.
    #[cfg(not(feature = "vst2"))]
    #[test]
    fn vst2_in_process_needs_its_feature() {
        for hosting in [Hosting::Auto, Hosting::InProcess] {
            let err = hosting
                .resolve(Path::new("P.vst"), PluginFormat::Vst2)
                .unwrap_err();
            assert!(matches!(
                err,
                BridgeError::NotInProcess { format: "VST2", .. }
            ));
        }
    }

    #[cfg(not(feature = "clap"))]
    #[test]
    fn clap_in_process_needs_its_feature() {
        let err = Hosting::InProcess
            .resolve(Path::new("P.clap"), PluginFormat::Clap)
            .unwrap_err();
        assert!(matches!(
            err,
            BridgeError::NotInProcess { format: "CLAP", .. }
        ));
    }

    #[cfg(feature = "clap")]
    #[test]
    fn clap_in_process_is_admitted_with_its_feature() {
        assert_eq!(
            Hosting::InProcess
                .resolve(Path::new("P.clap"), PluginFormat::Clap)
                .unwrap(),
            Hosting::InProcess
        );
    }

    /// **Where a plugin runs is not which plugin it is**: refs that differ
    /// only by hosting name one plugin (a state saved from one restores into
    /// the other), though they are not equal values.
    #[test]
    fn hosting_is_not_identity() {
        let served = entry_ref("/p/A.clap", PluginFormat::Clap, Some("a"));
        let here = served.clone().with_hosting(Hosting::InProcess);
        assert!(served.is_same_plugin(&here));
        assert_ne!(served, here);
        assert_eq!(here.hosting(), Hosting::InProcess);
        assert_eq!(served.hosting(), Hosting::Auto);
    }

    /// **The load chooses the plugin's id, in every format** (a VST2 shell
    /// holds several plugins too); no id is the file's first plugin.
    #[test]
    fn the_load_chooses_the_id() {
        for format in [
            PluginFormat::Clap,
            PluginFormat::Vst3,
            PluginFormat::Vst2,
            PluginFormat::AudioUnit,
        ] {
            let r = entry_ref("/p/x", format, Some("the.id"));
            assert_eq!(r.load(), Load::new().plugin("the.id"), "{format:?}");
            assert_eq!(entry_ref("/p/x", format, None).load(), Load::new());
        }
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

    /// **A link and the file it names are one plugin**, as a scan keys them
    /// (macOS's temp dir is such a link: `/var` to `/private/var`).
    #[cfg(unix)]
    #[test]
    fn a_link_and_its_file_are_one_plugin() {
        let dir = std::env::temp_dir().join(format!("tutti-ref-link-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("synth.clap");
        let link = dir.join("link.clap");
        std::fs::write(&file, b"x").unwrap();
        std::fs::remove_file(&link).ok();
        std::os::unix::fs::symlink(&file, &link).unwrap();
        let by_file = PluginRef::file(&file).unwrap();
        let by_link = PluginRef::file(&link).unwrap();
        assert!(by_file.is_same_plugin(&by_link));
        std::fs::remove_dir_all(&dir).ok();
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
