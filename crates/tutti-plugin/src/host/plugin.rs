//! [`Plugin`]: one loaded plugin, whatever format it is and wherever it runs.
//!
//! Loading returns one owned type rather than a `(node, PluginHandle)` pair
//! because the hostings are different concrete node types
//! (`PluginClient<Bound>` and `InProcClient`), and because a plugin is
//! more than its node: it also takes per-block MIDI, chord/scale context and
//! note expression, reached here through capability accessors.

use crate::host::plugin_ref::{Hosting, PluginRef};
use tutti_plugin_types::MainThread;

use crate::host::node::PluginClient;
#[cfg(any(
    feature = "vst2",
    feature = "clap",
    feature = "vst3",
    all(feature = "au", target_os = "macos")
))]
use tutti_plugin_db::PluginFormat;
#[cfg(any(
    feature = "vst2",
    feature = "clap",
    feature = "vst3",
    all(feature = "au", target_os = "macos")
))]
use tutti_plugin_host::InProcClient;
use tutti_plugin_host::PluginHandle;
use tutti_plugin_ipc::AudioConfig;
use tutti_plugin_types::BridgeResult;
use tutti_plugin_types::PluginRole;
use tutti_plugin_types::{Features, LoadedPlugin, PluginDescriptor};
use tutti_types::SampleRate;

/// The audio node, wherever it runs.
///
/// Private: which arm a plugin landed in is exactly the detail this type exists
/// to stop leaking ([`Plugin::hosting`] says where it runs, and nothing else
/// depends on it).
///
/// Every arm is boxed. An enum is as large as its largest variant, and these
/// differ by ~9 KiB (a served client is ~15 KiB, an in-process one ~6 KiB), so
/// an unboxed enum would make every `Plugin` (and every `BridgeResult`
/// carrying one) pay the larger figure.
enum Placed {
    Served(Box<PluginClient>),
    #[cfg(feature = "vst2")]
    Vst2(Box<InProcClient<tutti_vst2_host::Vst2Loaded>>),
    #[cfg(feature = "clap")]
    Clap(Box<InProcClient<tutti_clap_host::ClapLoaded>>),
    #[cfg(feature = "vst3")]
    Vst3(Box<InProcClient<tutti_vst3_host::Vst3Loaded>>),
    #[cfg(all(feature = "au", target_os = "macos"))]
    Au(Box<InProcClient<tutti_au_host::AuLoaded>>),
}

/// Runs `$in_process` on the client of whichever in-process format a [`Placed`]
/// holds, or `$served` on the served client: the one place the format arms are
/// listed for a call that does not change a client's type.
macro_rules! placed {
    ($placed:expr, served $s:ident => $served:expr, in_process $c:ident => $in_process:expr $(,)?) => {
        match $placed {
            Placed::Served($s) => $served,
            #[cfg(feature = "vst2")]
            Placed::Vst2($c) => $in_process,
            #[cfg(feature = "clap")]
            Placed::Clap($c) => $in_process,
            #[cfg(feature = "vst3")]
            Placed::Vst3($c) => $in_process,
            #[cfg(all(feature = "au", target_os = "macos"))]
            Placed::Au($c) => $in_process,
        }
    };
}

/// A loaded plugin: its audio node, its control surface, and the per-block
/// inputs it can accept.
///
/// Created by [`Plugin::open`] or `PluginLoad::run`.
/// Where a plugin runs is its ref's [`Hosting`]: by default VST3, CLAP and AU
/// plugins run in a `plugin-server` subprocess and VST2 (with the `vst2`
/// feature) in the host process; [`PluginRef::with_hosting`] asks for the other
/// where the format allows it. Which one a plugin landed in does not change
/// this API ([`hosting`](Self::hosting) says).
///
/// A plugin becomes a graph node by binding it to the project's tempo map
/// ([`bind`](Self::bind)): `editor.insert(key, kind, plugin.bind(tempo))`. Keep a clone of [`handle`](Self::handle)
/// first if you need the control surface afterwards; the plugin stays alive
/// while either the node or a handle does.
///
/// # Examples
///
/// ```no_run
/// use tutti_types::SampleRate;
/// use tutti_plugin::catalog::Plugin;
///
/// use tutti_plugin::catalog::PluginRef;
///
/// let file = PluginRef::file("/usr/lib/vst3/MyPlugin.vst3").expect("a plugin file");
/// let plugin = Plugin::open(None, &file, SampleRate::SR_48K)?;
/// println!("{} ({:?})", plugin.descriptor().name, plugin.role());
/// if plugin.takes_midi() {
///     // wire a MIDI source to the node's event input
/// }
/// # Ok::<(), tutti_plugin_types::BridgeError>(())
/// ```
pub struct Plugin {
    placed: Placed,
    handle: PluginHandle,
    /// The plugin that was opened, with the id the loaded plugin reports.
    plugin: PluginRef,
}

/// Reports identity and where the plugin runs, not the node's guts: the
/// hostings wrap live subprocess and FFI state that has no useful `Debug`.
impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("name", &self.descriptor().name)
            .field("format", &self.descriptor().class.format_name())
            .field("hosting", &self.hosting())
            .finish_non_exhaustive()
    }
}

impl Plugin {
    /// Opens a plugin file with the default bridge settings.
    ///
    /// No catalog is needed: discovery through [`Plugins`] is optional.
    /// ```no_run
    /// # fn ex() -> tutti_plugin_types::BridgeResult<()> {
    /// use tutti_plugin::catalog::{Plugin, PluginRef};
    /// let foo = PluginRef::file("/Library/Audio/Plug-Ins/VST3/Foo.vst3").expect("a plugin file");
    /// let plugin = Plugin::open(None, &foo, tutti_types::SampleRate::SR_48K)?;
    /// # Ok(()) }
    /// ```
    ///
    /// `sample_rate` takes anything convertible to [`SampleRate`] — `f64` and
    /// `u32` (which widens exactly), but deliberately not `f32`.
    ///
    /// `plugin` names the plugin: its file, and (from a scan,
    /// `PluginRef::from(&entry)`) which plugin of the file: a bundle that
    /// exports several opens the one the ref's id names, not the first.
    /// The format and the [`Hosting`] are the ref's. `Auto` runs VST2 (with
    /// the `vst2` feature) in the host process, since its `AEffect` fuses
    /// editor and audio processor into one instance, and every other format in
    /// a `plugin-server` subprocess with an IPC bridge. This call blocks while
    /// the subprocess starts and the plugin loads, so call it off the audio
    /// thread.
    ///
    /// `main` is the proof that the caller is on the main thread. Only an
    /// in-process load needs it, and refuses to load without it; a plugin
    /// served from a subprocess loads from any thread, so a caller that is not
    /// on the main thread passes `None`.
    ///
    /// **The blacklist is not consulted** — that is catalog state, and this
    /// path does not have one. A host that wants the scanner's crash history
    /// honoured should check [`Plugins::is_blacklisted`] before calling.
    ///
    /// # Errors
    ///
    /// Returns [`BridgeError::ServerNotFound`](tutti_plugin_types::BridgeError::ServerNotFound)
    /// when the `plugin-server` binary cannot be located,
    /// [`BridgeError::LoadFailed`](tutti_plugin_types::BridgeError::LoadFailed) or
    /// [`BridgeError::BundleResolutionFailed`](tutti_plugin_types::BridgeError::BundleResolutionFailed)
    /// when the plugin does not load, [`BridgeError::NotOutOfProcess`] for a
    /// VST2 plugin whose ref asks for [`Hosting::OutOfProcess`],
    /// [`BridgeError::NotInProcess`] for a format this build has no in-process
    /// host for, and a connection, protocol or timeout variant when the
    /// subprocess cannot be reached.
    ///
    /// [`BridgeError::NotOutOfProcess`]: tutti_plugin_types::BridgeError::NotOutOfProcess
    /// [`BridgeError::NotInProcess`]: tutti_plugin_types::BridgeError::NotInProcess
    ///
    /// [`Plugins`]: super::catalog::Catalog
    /// [`Plugins::is_blacklisted`]: super::catalog::Catalog::is_blacklisted
    /// [`AudioConfig`]: crate::BridgeConfig
    pub fn open(
        main: Option<&MainThread>,
        plugin: &PluginRef,
        sample_rate: impl Into<SampleRate>,
    ) -> BridgeResult<Self> {
        Self::open_with(main, &AudioConfig::default(), plugin, sample_rate)
    }

    /// Opens a plugin file with explicit bridge settings.
    ///
    /// See [`open`](Self::open).
    ///
    /// # Errors
    ///
    /// The same as [`open`](Self::open).
    pub(crate) fn open_with(
        main: Option<&MainThread>,
        audio: &AudioConfig,
        plugin: &PluginRef,
        sample_rate: impl Into<SampleRate>,
    ) -> BridgeResult<Self> {
        Self::open_with_state(main, audio, plugin, sample_rate, None).map(|(plugin, _)| plugin)
    }

    /// `open_with`, restoring `state` into the plugin
    /// **before it is activated**: every format takes a state on a plugin that is
    /// open and not yet active, and a plugin may take its mode from the state at
    /// activation. The second value is what became of it: `None` when none was
    /// given, else the plugin's refusal as text (the plugin is then loaded at its
    /// defaults, which beats no plugin).
    ///
    /// # Errors
    ///
    /// The same as [`open`](Self::open).
    pub fn open_with_state(
        main: Option<&MainThread>,
        audio: &AudioConfig,
        plugin: &PluginRef,
        sample_rate: impl Into<SampleRate>,
        state: Option<&[u8]>,
    ) -> BridgeResult<(Self, Option<std::result::Result<(), String>>)> {
        let sample_rate = sample_rate.into();
        let hosting = Self::hosting_of(plugin)?;
        if hosting == Hosting::InProcess {
            // An in-process plugin loads in this process, on the main thread:
            // the caller says it is there by passing the token.
            let Some(main) = main else {
                return Err(tutti_plugin_types::BridgeError::LoadFailed {
                    path: plugin.path().to_path_buf(),
                    stage: tutti_plugin_types::LoadStage::Opening,
                    reason: "a plugin hosted in the host process loads on the main thread \
                             and needs it"
                        .into(),
                });
            };
            return Self::open_in_process(main, plugin, sample_rate, state);
        }

        let ((client, restored), handle) =
            PluginClient::open(audio.to_bridge_config(), plugin, sample_rate, state).map(
                |(client, restored)| {
                    let handle = client.handle();
                    ((client, restored), handle)
                },
            )?;
        Ok((Self::from_subprocess(client, handle), restored))
    }

    /// Where a load of `plugin` runs: [`Hosting::InProcess`] or
    /// [`Hosting::OutOfProcess`], never `Auto`. An in-process load (and the
    /// instance's drop) is confined to the main thread; an out-of-process one
    /// loads from any thread.
    ///
    /// # Errors
    ///
    /// [`BridgeError::NotOutOfProcess`](tutti_plugin_types::BridgeError::NotOutOfProcess)
    /// for a VST2 plugin asked to be served,
    /// [`BridgeError::NotInProcess`](tutti_plugin_types::BridgeError::NotInProcess)
    /// for a format this build cannot host in process: the answer is the
    /// format's and the build's, so it comes before a server or a file is
    /// looked for.
    pub fn hosting_of(plugin: &PluginRef) -> BridgeResult<Hosting> {
        plugin.hosting().resolve(plugin.path(), plugin.format())
    }

    /// The in-process load, by format.
    #[allow(unused_variables)]
    fn open_in_process(
        main: &MainThread,
        plugin: &PluginRef,
        sample_rate: SampleRate,
        state: Option<&[u8]>,
    ) -> BridgeResult<(Self, Option<std::result::Result<(), String>>)> {
        match plugin.format() {
            #[cfg(feature = "vst2")]
            PluginFormat::Vst2 => Self::load_in_process::<tutti_vst2_host::Vst2Loaded>(
                main,
                plugin,
                sample_rate,
                state,
                Placed::Vst2,
            ),
            #[cfg(feature = "clap")]
            PluginFormat::Clap => Self::load_in_process::<tutti_clap_host::ClapLoaded>(
                main,
                plugin,
                sample_rate,
                state,
                Placed::Clap,
            ),
            #[cfg(feature = "vst3")]
            PluginFormat::Vst3 => Self::load_in_process::<tutti_vst3_host::Vst3Loaded>(
                main,
                plugin,
                sample_rate,
                state,
                Placed::Vst3,
            ),
            #[cfg(all(feature = "au", target_os = "macos"))]
            PluginFormat::AudioUnit => Self::load_in_process::<tutti_au_host::AuLoaded>(
                main,
                plugin,
                sample_rate,
                state,
                Placed::Au,
            ),
            // `hosting_of` refused every format with no host built in; this
            // arm is the same refusal, for a caller that skipped it.
            _ => Err(tutti_plugin_types::BridgeError::NotInProcess {
                path: plugin.path().to_path_buf(),
                format: "plugin",
            }),
        }
    }

    /// Loads the format `F` in this process, restoring `state` before it is
    /// activated, and wraps its client as `place`.
    #[cfg(any(
        feature = "vst2",
        feature = "clap",
        feature = "vst3",
        all(feature = "au", target_os = "macos")
    ))]
    fn load_in_process<F>(
        main: &MainThread,
        plugin: &PluginRef,
        sample_rate: SampleRate,
        state: Option<&[u8]>,
        place: fn(Box<InProcClient<F>>) -> Placed,
    ) -> BridgeResult<(Self, Option<std::result::Result<(), String>>)>
    where
        F: tutti_plugin_types::InProcess + 'static,
        F::Processor: 'static,
    {
        let mut restore = state.map(|data| {
            move |loaded: &mut F| {
                tutti_plugin_types::Control::load_state(
                    loaded,
                    main,
                    data,
                    tutti_plugin_types::StateUse::Project,
                )
                .map_err(|e| e.to_string())
            }
        });
        let (client, handle, restored) = tutti_plugin_host::load_client_restoring(
            main,
            plugin.path(),
            &plugin.load(),
            sample_rate,
            restore.as_mut().map(|r| r as _),
        )?;
        let plugin = plugin.identified(&handle.descriptor().id);
        Ok((
            Self {
                placed: place(Box::new(client)),
                handle,
                plugin,
            },
            restored,
        ))
    }

    /// Wrap a subprocess client.
    fn from_subprocess(client: PluginClient, handle: PluginHandle) -> Self {
        Self {
            plugin: client.plugin().clone(),
            placed: Placed::Served(Box::new(client)),
            handle,
        }
    }

    /// Where the plugin runs: [`Hosting::InProcess`] or
    /// [`Hosting::OutOfProcess`], never `Auto`.
    pub fn hosting(&self) -> Hosting {
        match &self.placed {
            Placed::Served(_) => Hosting::OutOfProcess,
            #[allow(unreachable_patterns)]
            _ => Hosting::InProcess,
        }
    }

    // ---- Identity ---------------------------------------------------------

    /// The plugin that was opened: the file, the format, and the id the loaded
    /// plugin reports (filled in when the open was by file alone).
    pub fn plugin(&self) -> &PluginRef {
        &self.plugin
    }

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
    pub fn set_render_mode(&self, mode: tutti_plugin_types::RenderMode) -> bool {
        if !self.accepts(Features::RENDER_MODE) {
            return false;
        }
        placed!(&self.placed, served c => c.set_render_mode(mode), in_process c => c.set_render_mode(mode))
    }

    /// Gives the plugin the project meter, for the time signature and bar of
    /// the transport it is told. The node's meter follows the project meter
    /// the host publishes edits to, so an edit reaches the running plugin
    /// with nothing re-installed.
    ///
    /// The transport itself needs no install: every plugin node reads it from
    /// each block's graph context once it is in a graph (the transport its
    /// input was rendered at).
    ///
    /// Returns `false` if the plugin declared it does not want a transport
    /// snapshot.
    #[must_use = "a false return means the plugin declined this input and nothing was installed"]
    pub fn set_meter(&self, meter: tutti_transport::ProjectMeter) -> bool {
        if !self.accepts(Features::TRANSPORT) {
            return false;
        }
        self.meter().set_meter(meter);
        true
    }

    /// Drops the meter; the plugin is told 4/4 from bar 0.
    pub fn clear_meter(&self) {
        self.meter().clear_meter();
    }

    /// The node's meter cell, as a handle of its own, for either hosting:
    /// what a host keeps to install the meter after the plugin has moved
    /// into a graph (the same cell [`set_meter`](Self::set_meter) writes).
    pub fn meter(&self) -> tutti_plugin_host::PluginMeter {
        placed!(&self.placed, served c => c.controls().meter(), in_process c => c.controls().meter())
    }

    /// What the node counts and holds, for either hosting: its meter cell and
    /// the delivery counters (events it dropped, parameter points it sent,
    /// merged or refused). The same cells the node writes, so a host keeps
    /// this to read them after the plugin has moved into a graph.
    pub fn node_controls(&self) -> tutti_plugin_host::NodeControls {
        placed!(&self.placed, served c => (*c.controls()).clone(), in_process c => c.controls())
    }

    /// Returns whether the plugin takes chord and scale context (it declared
    /// sequencer context).
    ///
    /// When `true`, wire a `HarmonyNode` (from `tutti-midi-runtime`) to its
    /// event input. `false` for a plugin that declined, and for an in-process
    /// node, whose event input takes only MIDI.
    pub fn takes_harmony(&self) -> bool {
        placed!(&self.placed, served c => c.takes_harmony(), in_process _c => false)
    }

    // ---- The graph node ---------------------------------------------------

    /// Binds the plugin to the reader of the project's tempo map it tells the
    /// plugin the tempo from (and CLAP its slope), wherever it
    /// runs: [`PluginClient::bind`] for a subprocess plugin,
    /// `InProcClient::bind` for an in-process one. The result is the graph
    /// node ([`BoundPlugin`]); `Session::add_plugin` binds every plugin with
    /// the session's reader, and a host with no session passes a
    /// [`TempoReader::fixed`](tutti_transport::TempoReader::fixed) one.
    #[must_use]
    pub fn bind(self, tempo: tutti_transport::TempoReader) -> BoundPlugin {
        let bound = match self.placed {
            Placed::Served(c) => BoundPlaced::Served(Box::new(c.bind(tempo))),
            #[cfg(feature = "vst2")]
            Placed::Vst2(c) => BoundPlaced::Vst2(Box::new(c.bind(tempo))),
            #[cfg(feature = "clap")]
            Placed::Clap(c) => BoundPlaced::Clap(Box::new(c.bind(tempo))),
            #[cfg(feature = "vst3")]
            Placed::Vst3(c) => BoundPlaced::Vst3(Box::new(c.bind(tempo))),
            #[cfg(all(feature = "au", target_os = "macos"))]
            Placed::Au(c) => BoundPlaced::Au(Box::new(c.bind(tempo))),
        };
        BoundPlugin { bound }
    }

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
    /// Returns the plugin itself, unchanged (boxed, as the client is), when it
    /// is not a `PluginClient`: an in-process instance. Bind it with
    /// [`bind`](Self::bind) and insert the [`BoundPlugin`]
    /// (`editor.insert(key, kind, plugin.bind(tempo))`), which hands the
    /// editor its fork source (a fork by state transfer, in this process).
    pub fn into_client(self) -> std::result::Result<Box<PluginClient>, Box<Self>> {
        match self.placed {
            Placed::Served(c) => Ok(c),
            #[allow(unreachable_patterns)]
            _ => Err(Box::new(self)),
        }
    }
}

/// A [`Plugin`] bound to the project's tempo map ([`Plugin::bind`]): the graph
/// node, wherever it runs: `editor.insert(key, kind,
/// plugin.bind(tempo))`.
///
/// Consuming and explicit: a plugin *has* a node, it is not one. Wire the
/// per-block inputs before inserting. The [`PluginHandle`] survives
/// independently; clone it from [`handle`](Plugin::handle) first if the
/// control surface is needed after the node moves into the graph.
///
/// The controls are the subprocess node's [`PluginControls`]; `None` for an
/// in-process plugin, whose meter is reached through [`Plugin::meter`]
/// (take it before binding; its own `IntoNode` hands back its
/// `NodeControls`, which carry its parameter counters too). Either
/// hands the editor its fork source: a fork by state transfer, into a new
/// `plugin-server` for a subprocess plugin, and a second instance in this
/// process for an in-process VST2 one.
///
/// [`PluginControls`]: tutti_plugin_host::PluginControls
pub struct BoundPlugin {
    bound: BoundPlaced,
}

enum BoundPlaced {
    Served(Box<PluginClient<crate::host::node::Bound>>),
    #[cfg(feature = "vst2")]
    Vst2(Box<tutti_plugin_host::PluginNode<tutti_vst2_host::Vst2Loaded>>),
    #[cfg(feature = "clap")]
    Clap(Box<tutti_plugin_host::PluginNode<tutti_clap_host::ClapLoaded>>),
    #[cfg(feature = "vst3")]
    Vst3(Box<tutti_plugin_host::PluginNode<tutti_vst3_host::Vst3Loaded>>),
    #[cfg(all(feature = "au", target_os = "macos"))]
    Au(Box<tutti_plugin_host::PluginNode<tutti_au_host::AuLoaded>>),
}

impl std::fmt::Debug for BoundPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundPlugin").finish_non_exhaustive()
    }
}

impl tutti_graph::IntoNode for BoundPlugin {
    type Controls = Option<tutti_plugin_host::PluginControls>;

    fn into_node(self) -> (Box<dyn tutti_graph::Node>, Self::Controls) {
        let tutti_graph::NodeParts { node, controls, .. } = self.into_parts();
        (node, controls)
    }

    fn into_parts(self) -> tutti_graph::NodeParts<Self::Controls> {
        // An in-process node has no `PluginControls` (its meter is reached
        // through `Plugin::meter`, taken before binding).
        #[cfg(any(
            feature = "vst2",
            feature = "clap",
            feature = "vst3",
            all(feature = "au", target_os = "macos")
        ))]
        macro_rules! in_process {
            ($c:expr) => {{
                let parts = tutti_graph::IntoNode::into_parts(*$c);
                tutti_graph::NodeParts {
                    node: parts.node,
                    controls: None,
                    fork: parts.fork,
                }
            }};
        }
        match self.bound {
            BoundPlaced::Served(c) => {
                let parts = tutti_graph::IntoNode::into_parts(*c);
                tutti_graph::NodeParts {
                    node: parts.node,
                    controls: Some(parts.controls),
                    fork: parts.fork,
                }
            }
            #[cfg(feature = "vst2")]
            BoundPlaced::Vst2(c) => in_process!(c),
            #[cfg(feature = "clap")]
            BoundPlaced::Clap(c) => in_process!(c),
            #[cfg(feature = "vst3")]
            BoundPlaced::Vst3(c) => in_process!(c),
            #[cfg(all(feature = "au", target_os = "macos"))]
            BoundPlaced::Au(c) => in_process!(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::host::node::is_declined;
    use tutti_plugin_types::{Features, LoadedPlugin};

    /// `Plugin`'s capability gate is the one the node accessors use.
    ///
    /// The installers on `Plugin` dispatch across the hostings, so they cannot
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
