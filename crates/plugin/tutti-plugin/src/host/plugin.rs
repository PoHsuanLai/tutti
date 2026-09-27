//! [`Plugin`]: one loaded plugin, whatever format it is and wherever it runs.
//!
//! Loading returns one owned type rather than a `(node, PluginHandle)` pair
//! because the two backends are different concrete node types
//! (`PluginClient<Bound>` and `InProcessVst2Client`), and because a plugin is
//! more than its node: it also takes per-block MIDI, chord/scale context and
//! note expression, reached here through capability accessors.

use std::path::Path;
use std::sync::Arc;

use crate::error::Result;
use crate::host::discovery::record::PluginRole;
use crate::host::discovery::{format_from_path, record::PluginFormat};
use crate::host::handles::PluginHandle;
use crate::host::node::PluginClient;
use crate::protocol::{Features, LoadedPlugin, PluginDescriptor};
use crate::util::config::AudioConfig;
use tutti_core::SampleRate;

/// The audio node, whichever backend produced it.
///
/// Private: which arm a plugin landed in is exactly the detail this type exists
/// to stop leaking.
enum Backend {
    /// Both arms are boxed. An enum is as large as its largest variant, and
    /// these differ by ~9 KiB (a subprocess client is ~15 KiB, an in-process
    /// one ~6 KiB), so an unboxed enum would make every `Plugin` — and every
    /// `Result` carrying one — pay the larger figure.
    Subprocess(Box<PluginClient>),
    #[cfg(feature = "vst2")]
    InProcessVst2(Box<crate::format::vst2_in_process::InProcessVst2Client>),
}

/// A loaded plugin: its audio node, its control surface, and the per-block
/// inputs it can accept.
///
/// Created by [`Plugin::open`] or [`Plugins::open`](super::plugins::Plugins::open).
/// VST3, CLAP and AU plugins run in a `plugin-server` subprocess; with the
/// `vst2` feature, VST2 plugins run in the host process. Which backend a plugin
/// landed in does not change this API: where one cannot do something, the
/// accessor answers `false` or `None`.
///
/// `Plugin` is a [`tutti_graph::IntoNode`], so inserting it into a graph is
/// `editor.insert(key, kind, plugin)`. Keep a clone of [`handle`](Self::handle)
/// first if you need the control surface afterwards; the plugin stays alive
/// while either the node or a handle does.
///
/// # Examples
///
/// ```no_run
/// use tutti_core::SampleRate;
/// use tutti_plugin::catalog::Plugin;
///
/// let plugin = Plugin::open("/usr/lib/vst3/MyPlugin.vst3", SampleRate::new(48_000.0))?;
/// println!("{} ({:?})", plugin.descriptor().name, plugin.role());
/// if plugin.takes_midi() {
///     // wire a MIDI source to the node's event input
/// }
/// # Ok::<(), tutti_plugin::BridgeError>(())
/// ```
pub struct Plugin {
    backend: Backend,
    handle: PluginHandle,
}

/// Reports identity and where the plugin runs, not the node's guts: the
/// backends wrap live subprocess and FFI state that has no useful `Debug`.
impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("name", &self.descriptor().name)
            .field("format", &self.descriptor().class.format_name())
            .field(
                "hosting",
                &match &self.backend {
                    Backend::Subprocess(_) => "subprocess",
                    #[cfg(feature = "vst2")]
                    Backend::InProcessVst2(_) => "in-process",
                },
            )
            .finish_non_exhaustive()
    }
}

impl Plugin {
    /// Opens a plugin file with the default bridge settings.
    ///
    /// No catalog is needed: discovery through [`Plugins`] is optional.
    /// ```no_run
    /// # fn ex() -> tutti_plugin::Result<()> {
    /// use tutti_plugin::catalog::Plugin;
    /// let plugin = Plugin::open("/Library/Audio/Plug-Ins/VST3/Foo.vst3", 48_000.0)?;
    /// # Ok(()) }
    /// ```
    ///
    /// `sample_rate` takes anything convertible to [`SampleRate`] — `f64` and
    /// `u32` (which widens exactly), but deliberately not `f32`.
    ///
    /// The format is taken from the path's extension. VST2 (with the `vst2`
    /// feature) runs in the host process, since its `AEffect` fuses editor and
    /// audio processor into one instance; every other format runs in a
    /// `plugin-server` subprocess with an IPC bridge. This call blocks while
    /// the subprocess starts and the plugin loads, so call it off the audio
    /// thread.
    ///
    /// **The blacklist is not consulted** — that is catalog state, and this
    /// path does not have one. A host that wants the scanner's crash history
    /// honoured should check [`Plugins::is_blacklisted`] before calling.
    ///
    /// # Errors
    ///
    /// Returns [`BridgeError::ServerNotFound`](crate::BridgeError::ServerNotFound)
    /// when the `plugin-server` binary cannot be located,
    /// [`BridgeError::LoadFailed`](crate::BridgeError::LoadFailed) or
    /// [`BridgeError::BundleResolutionFailed`](crate::BridgeError::BundleResolutionFailed)
    /// when the plugin does not load, and a connection, protocol or timeout
    /// variant when the subprocess cannot be reached.
    ///
    /// [`Plugins`]: super::plugins::Plugins
    /// [`Plugins::is_blacklisted`]: super::plugins::Plugins::is_blacklisted
    /// [`AudioConfig`]: crate::BridgeConfig
    pub fn open(path: impl AsRef<Path>, sample_rate: impl Into<SampleRate>) -> Result<Self> {
        Self::open_with(&AudioConfig::default(), path.as_ref(), sample_rate)
    }

    /// Opens a plugin file with explicit bridge settings.
    ///
    /// See [`open`](Self::open).
    ///
    /// # Errors
    ///
    /// The same as [`open`](Self::open).
    pub fn open_with(
        audio: &AudioConfig,
        path: impl AsRef<Path>,
        sample_rate: impl Into<SampleRate>,
    ) -> Result<Self> {
        let path = path.as_ref();
        let sample_rate = sample_rate.into();

        #[cfg(feature = "vst2")]
        if matches!(format_from_path(path), Some(PluginFormat::Vst2)) {
            let (client, handle) = crate::format::vst2_in_process::load_client(path, sample_rate)?;
            return Ok(Self::from_in_process_vst2(client, handle));
        }
        let _ = format_from_path; // keep the import live without the vst2 feature
        let _ = PluginFormat::Vst2;

        let (client, handle) =
            PluginClient::new(audio.to_bridge_config(), path.to_path_buf(), sample_rate).map(
                |client| {
                    let handle = PluginHandle::from_client(&client);
                    (client, handle)
                },
            )?;
        Ok(Self::from_subprocess(client, handle))
    }

    /// Wrap a subprocess client.
    fn from_subprocess(client: PluginClient, handle: PluginHandle) -> Self {
        Self {
            backend: Backend::Subprocess(Box::new(client)),
            handle,
        }
    }

    /// Wrap an in-process VST2 client.
    #[cfg(feature = "vst2")]
    fn from_in_process_vst2(
        client: crate::format::vst2_in_process::InProcessVst2Client,
        handle: PluginHandle,
    ) -> Self {
        Self {
            backend: Backend::InProcessVst2(Box::new(client)),
            handle,
        }
    }

    // ---- Identity ---------------------------------------------------------

    /// Returns the main-thread control surface: editor, parameters, state.
    ///
    /// Cheap to clone, and shares the plugin's lifetime — the plugin dies when
    /// the last handle *or* `Plugin` drops.
    pub fn handle(&self) -> &PluginHandle {
        &self.handle
    }

    /// Returns the catalog identity: name, vendor, native classification.
    pub fn descriptor(&self) -> &PluginDescriptor {
        self.handle.descriptor()
    }

    /// Returns the load-time wiring: bus widths, latency, tail, capabilities.
    pub fn loaded(&self) -> &LoadedPlugin {
        self.handle.loaded()
    }

    /// Returns what this plugin is (instrument, effect, …), normalized across
    /// formats.
    pub fn role(&self) -> PluginRole {
        self.descriptor().class.role()
    }

    // ---- Per-block inputs -------------------------------------------------

    /// Whether the plugin accepts this input, i.e. did not answer `false`.
    ///
    /// Shares `is_declined` with the [`PluginClient`] accessors, so the two
    /// agree about what an unprobed capability means.
    fn accepts(&self, f: Features) -> bool {
        !crate::host::node::is_declined(self.loaded(), f)
    }

    /// Returns whether the plugin takes MIDI, that is, it did not decline MIDI
    /// input.
    ///
    /// When `true`, wire MIDI to its node's event input.
    pub fn takes_midi(&self) -> bool {
        self.accepts(Features::MIDI_IN)
    }

    /// Returns whether the plugin sends MIDI, that is, it did not decline MIDI
    /// output.
    ///
    /// When `true`, its node's event output carries it.
    pub fn sends_midi(&self) -> bool {
        self.accepts(Features::MIDI_OUT)
    }

    /// Tells the plugin whether it is being rendered under realtime pressure.
    ///
    /// Set this **before** pulling blocks for an offline bounce: a plugin may
    /// spend more per block when it knows there is no deadline, and three of
    /// the four formats can only take the change while the plugin is
    /// deactivated. Leaving it unset renders the live-quality result into a
    /// file the user asked to be exact.
    ///
    /// `false` if the plugin declared it does not honour a render-mode change
    /// — a CLAP plugin that does not implement `clap.render`, or an AU without
    /// `kAudioUnitProperty_OfflineRender`. That is a refusal, not a failure:
    /// such a plugin renders identically either way, which is exactly what
    /// declining the extension means.
    #[must_use = "a false return means the plugin declined the render mode and nothing was applied"]
    pub fn set_render_mode(&self, mode: crate::protocol::RenderMode) -> bool {
        if !self.accepts(Features::RENDER_MODE) {
            return false;
        }
        match &self.backend {
            Backend::Subprocess(c) => c.set_render_mode(mode),
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(c) => c.set_render_mode(mode),
        }
    }

    /// Gives the plugin the project transport and meter.
    ///
    /// Returns `false` if the plugin declared it does not want a transport
    /// snapshot.
    ///
    /// The subprocess node reads the transport from each block's `Env` once it
    /// is in a graph, so it takes only `meter` and ignores `reader`; the
    /// in-process VST2 node polls `reader` into the `TimeInfo` its
    /// `audioMasterGetTime` callback serves.
    #[must_use = "a false return means the plugin declined this input and nothing was installed"]
    pub fn set_transport_source(
        &mut self,
        reader: tutti_core::transport::Transport,
        meter: Arc<tutti_core::RtPublish<tutti_core::meter::MeterMap>>,
    ) -> bool {
        if !self.accepts(Features::TRANSPORT) {
            return false;
        }
        match &mut self.backend {
            Backend::Subprocess(c) => {
                let _ = reader;
                c.set_meter(meter);
            }
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(c) => c.set_transport_source(reader, meter),
        }
        true
    }

    /// Returns whether the plugin takes chord and scale context (it declared
    /// sequencer context).
    ///
    /// When `true`, wire a `HarmonyNode` (from `tutti-midi-runtime`) to its
    /// event input. `false` for a plugin that declined, and for the in-process
    /// VST2 node, whose event input takes only MIDI.
    pub fn takes_harmony(&self) -> bool {
        match &self.backend {
            Backend::Subprocess(c) => c.takes_harmony(),
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(_) => false,
        }
    }

    /// Creates a sample-accurate parameter automation source, one curve per
    /// parameter.
    ///
    /// The returned [`PluginAutomation`](crate::handles::PluginAutomation) is
    /// an event source node: insert it and wire it to this plugin node's event
    /// input ([`PluginControls::automation`](crate::handles::PluginControls::automation)).
    ///
    /// No capability gate: every format carries parameter automation. `None`
    /// for the in-process VST2 node, whose event input takes only MIDI (its
    /// parameters are driven through the handle instead).
    pub fn automation(
        &self,
        params: impl IntoIterator<Item = crate::host::node::TimedParam>,
    ) -> Option<crate::host::node::PluginAutomation> {
        match &self.backend {
            Backend::Subprocess(c) => Some(c.automation(params)),
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(_) => None,
        }
    }

    // ---- The graph node ---------------------------------------------------

    /// Returns the node as the concrete out-of-process [`PluginClient`],
    /// unbound.
    ///
    /// For a host that wants the typed client: its
    /// [`controls`](PluginClient::controls) before insertion, or its own
    /// insertion path ([`bind`](PluginClient::bind) then `Editor::insert`,
    /// which hands back its controls and the fork source that forks it by
    /// state transfer).
    ///
    /// Clone the [`handle`](Self::handle) first if the control surface is
    /// needed after this. Boxed, because a `PluginClient` is several KiB.
    ///
    /// # Errors
    ///
    /// Returns the graph node for a plugin that is not a `PluginClient` — an
    /// in-process VST2 instance, which has no fork source: insert it as
    /// `tutti_graph::Unforkable(node)`, and a fork of a graph holding it is
    /// refused as not forkable.
    pub fn into_client(self) -> std::result::Result<Box<PluginClient>, Box<dyn tutti_graph::Node>> {
        match self.backend {
            Backend::Subprocess(c) => Ok(c),
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(c) => Err(c),
        }
    }
}

/// A loaded plugin as a graph node, whichever backend it landed in:
/// `editor.insert(key, kind, plugin)`.
///
/// Consuming and explicit: a plugin *has* a node, it is not one. Wire the
/// per-block inputs before inserting — they are installed on shared cells, so
/// an install afterwards through the returned controls still reaches the
/// node, but keeping the order obvious is the point of the API. The
/// [`PluginHandle`] survives independently; clone it from
/// [`handle`](Plugin::handle) first if the control surface is needed after
/// the node moves into the graph.
///
/// The controls are the subprocess node's [`PluginControls`]; `None` for an
/// in-process VST2 plugin, whose per-block inputs are installed through
/// `Plugin` before insertion. A subprocess plugin hands the editor its fork
/// source (a fork by state transfer); an in-process VST2 one has none.
///
/// [`PluginControls`]: crate::host::node::PluginControls
impl tutti_graph::IntoNode for Plugin {
    type Controls = Option<crate::host::node::PluginControls>;

    fn into_node(self) -> (Box<dyn tutti_graph::Node>, Self::Controls) {
        let tutti_graph::NodeParts { node, controls, .. } = self.into_parts();
        (node, controls)
    }

    fn into_parts(self) -> tutti_graph::NodeParts<Self::Controls> {
        match self.backend {
            Backend::Subprocess(c) => {
                let parts = tutti_graph::IntoNode::into_parts(c.bind());
                tutti_graph::NodeParts {
                    node: parts.node,
                    controls: Some(parts.controls),
                    fork: parts.fork,
                }
            }
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(c) => {
                let parts = tutti_graph::IntoNode::into_parts(*c);
                tutti_graph::NodeParts {
                    node: parts.node,
                    controls: None,
                    fork: parts.fork,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::host::node::is_declined;
    use crate::protocol::{Features, LoadedPlugin};

    /// `Plugin`'s capability gate is the one the node accessors use.
    ///
    /// The installers on `Plugin` dispatch across two backends, so they cannot
    /// share the accessors themselves — but they must not restate the rule.
    /// This pins that the shared predicate is what both consult, so a change to
    /// "declined" reaches the unified surface too.
    #[test]
    fn the_unified_surface_gates_on_the_same_predicate_as_the_node() {
        let declined = LoadedPlugin {
            probed: Features::TRANSPORT,
            features: Features::empty(),
            ..Default::default()
        };
        assert!(is_declined(&declined, Features::TRANSPORT));

        let advertised = LoadedPlugin {
            probed: Features::TRANSPORT,
            features: Features::TRANSPORT,
            ..Default::default()
        };
        assert!(!is_declined(&advertised, Features::TRANSPORT));

        // Unprobed stays open.
        assert!(!is_declined(&LoadedPlugin::default(), Features::TRANSPORT));
    }
}
