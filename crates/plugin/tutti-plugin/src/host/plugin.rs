//! [`Plugin`] — one loaded plugin, whatever format it is and wherever it runs.
//!
//! # Why this type exists
//!
//! Loading returns one owned type rather than a `(node, PluginHandle)` pair.
//! Two reasons, the first of which is why the boxing:
//!
//! **The process boundary leaked.** VST2 runs in the host process (its `AEffect`
//! fuses editor and audio processor into one instance, so the two cannot be
//! split across processes); every other format runs in a subprocess. Those are
//! different concrete types — a native graph node (`PluginClient<Bound>`)
//! and an `AudioUnit` run through `tutti_graph::Legacy` — so the only thing
//! a single return could name was a boxed node. Whether a plugin was
//! in-process is an implementation detail, and it decided the caller's API
//! surface.
//!
//! **A node misdescribes a plugin.** It says "a compute unit with N audio
//! inputs". But a plugin also consumes per-block MIDI, chord/scale context and
//! note-expression through side-channels the graph does not carry yet — so a
//! synth has no audio inputs while consuming a MIDI stream every block.
//! Handing back the node as the plugin's identity discards everything else it
//! is.
//!
//! `Plugin` owns the node privately and gives it up as one deliberate step:
//! it is itself an [`IntoNode`](tutti_graph::IntoNode), so inserting it into
//! a graph is `editor.insert(key, kind, plugin)`. The per-block
//! streams are reached through capability accessors that answer `None` when the
//! plugin declined them, so the check is the shape of the call rather than
//! something to remember.
//!
//! # The two backends answer the same questions
//!
//! [`PluginHandle`] was already unified this way — both loaders build one, and
//! where a backend cannot do something the answer is an `Option`
//! (`automation_state()` is `None` for in-process VST2) rather than a second
//! type. This applies that to the node half.
//!
//! Where the two genuinely differ, the difference is a *format* capability and
//! not a process-boundary one:
//!
//! - MIDI and transport unify — both backends own the same `Midi`, and both
//!   map the transport through the same function (`transport_source`); the
//!   subprocess node reads it from the graph's `Env`, the in-process VST2 node
//!   (an `AudioUnit` until it is ported) from a polled reader.
//! - Harmony, note-expression and param automation are subprocess-only, and
//!   VST2 has no such concepts to begin with, so declining them is the honest
//!   answer for the format rather than an artifact of where it runs.

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
/// See the module docs for why this replaces
/// `(Box<dyn AudioUnit>, PluginHandle)`.
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
    /// Open a plugin file.
    ///
    /// Takes a path, not a catalog entry: opening reads the file and the bridge
    /// settings and nothing else, so requiring a [`Plugins`] first was an
    /// artifact of where [`AudioConfig`] happened to be stored. Discovery — the
    /// catalog's actual job — stays optional.
    ///
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
    /// Format dispatch is internal. VST2 (with the `vst2` feature) runs in the
    /// host process, since its `AEffect` fuses editor and audio processor into
    /// one instance; every other format runs in a subprocess with an IPC
    /// bridge. Which of the two a plugin landed in does not change the API.
    ///
    /// **The blacklist is not consulted** — that is catalog state, and this
    /// path does not have one. A host that wants the scanner's crash history
    /// honoured should check [`Plugins::is_blacklisted`] before calling.
    ///
    /// [`Plugins`]: super::plugins::Plugins
    /// [`Plugins::is_blacklisted`]: super::plugins::Plugins::is_blacklisted
    /// [`AudioConfig`]: crate::BridgeConfig
    pub fn open(path: impl AsRef<Path>, sample_rate: impl Into<SampleRate>) -> Result<Self> {
        Self::open_with(&AudioConfig::default(), path.as_ref(), sample_rate)
    }

    /// [`open`](Self::open) with explicit bridge settings.
    ///
    /// Separate rather than a builder because the settings are one plain
    /// struct a host already holds — `AudioConfig::default()` covers every
    /// caller in this tree, and a builder would be designing for a
    /// configuration nobody has needed yet.
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

    /// The main-thread control surface: editor, parameters, state.
    ///
    /// Cheap to clone, and shares the plugin's lifetime — the plugin dies when
    /// the last handle *or* `Plugin` drops.
    pub fn handle(&self) -> &PluginHandle {
        &self.handle
    }

    /// Catalog identity — name, vendor, native classification.
    pub fn descriptor(&self) -> &PluginDescriptor {
        self.handle.descriptor()
    }

    /// Load-time engine wiring: bus widths, latency, tail, capabilities.
    pub fn loaded(&self) -> &LoadedPlugin {
        self.handle.loaded()
    }

    /// What this plugin is, normalized across formats.
    ///
    /// Answers the placement question — which subsystem should adopt it —
    /// separately from the wiring question the accessors below answer.
    pub fn role(&self) -> PluginRole {
        self.descriptor().class.role()
    }

    // ---- Per-block inputs -------------------------------------------------

    /// Whether the plugin accepts this input, i.e. did not answer `false`.
    ///
    /// Shares [`is_declined`] with the [`PluginClient`] accessors rather than
    /// restating the rule, so the two cannot drift into disagreeing about what
    /// an unprobed capability means. See that module for why `None` stays open.
    fn accepts(&self, f: Features) -> bool {
        !crate::host::node::is_declined(self.loaded(), f)
    }

    /// Producer handle for this plugin's MIDI inbox, or `None` if it declared
    /// no MIDI input. Cheap to clone; push events as they arrive.
    pub fn midi_sender(&self) -> Option<tutti_midi_runtime::MidiSender> {
        if !self.accepts(Features::MIDI_IN) {
            return None;
        }
        Some(match &self.backend {
            Backend::Subprocess(c) => c.midi_sender(),
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(c) => c.midi_sender(),
        })
    }

    /// Install a transport-aware MIDI source polled once per block, layered
    /// over the live inbox. `false` if the plugin declared no MIDI input.
    ///
    /// This is the clip-playback path; [`midi_sender`](Self::midi_sender) is the
    /// live one. They coexist — the port drains both.
    #[must_use = "a false return means the plugin declined this input and nothing was installed"]
    pub fn set_midi_source(&mut self, source: Arc<dyn tutti_midi_types::MidiUnitIn>) -> bool {
        if !self.accepts(Features::MIDI_IN) {
            return false;
        }
        match &mut self.backend {
            Backend::Subprocess(c) => c.set_midi_source(source),
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(c) => c.set_midi_source(source),
        }
        true
    }

    /// Drop a previously-installed MIDI source; subsequent blocks poll only the
    /// live inbox again.
    ///
    /// Returns nothing, unlike its installing counterpart: there is no input to
    /// decline, and clearing a plugin that was never routed is already the
    /// no-op the caller wants.
    pub fn clear_midi_source(&mut self) {
        match &mut self.backend {
            Backend::Subprocess(c) => c.clear_midi_source(),
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(c) => c.clear_midi_source(),
        }
    }

    /// Route this plugin's MIDI-out back into the graph. `false` if it declared
    /// no MIDI output.
    #[must_use = "a false return means the plugin declined this input and nothing was installed"]
    pub fn set_midi_out(&self, sink: Arc<tutti_midi_runtime::MidiOutSink>) -> bool {
        if !self.accepts(Features::MIDI_OUT) {
            return false;
        }
        match &self.backend {
            Backend::Subprocess(c) => c.set_midi_out(sink),
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(c) => c.set_midi_out(sink),
        }
        true
    }

    /// Drop the outbound routing target; subsequent blocks discard MIDI-out.
    ///
    /// Returns nothing, for the same reason as
    /// [`clear_midi_source`](Self::clear_midi_source).
    pub fn clear_midi_out(&self) {
        match &self.backend {
            Backend::Subprocess(c) => c.clear_midi_out(),
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(c) => c.clear_midi_out(),
        }
    }

    /// Tell the plugin whether it is being rendered under realtime pressure.
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

    /// Give the plugin the project transport and meter. `false` if the plugin
    /// declared it does not want a transport snapshot.
    ///
    /// Both backends deliver it, from different places. The subprocess node
    /// reads the transport from each block's `Env` once it is in a graph, so
    /// it takes only `meter` and `reader` is not used; the in-process VST2 node
    /// has no `Env` (it is an `AudioUnit` until it is ported, doc 013) and
    /// polls `reader` into the `TimeInfo` its `audioMasterGetTime` callback
    /// serves.
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

    /// Install per-block chord/scale context. `false` if the plugin declared no
    /// sequencer context.
    #[must_use = "a false return means the plugin declined this input and nothing was installed"]
    pub fn set_harmony_source(
        &mut self,
        chords: impl IntoIterator<Item = crate::host::node::TimedChord>,
        scales: impl IntoIterator<Item = crate::host::node::TimedScale>,
        transport: impl tutti_core::transport::Timeline + 'static,
    ) -> bool {
        if !self.accepts(Features::SEQUENCER_CONTEXT) {
            return false;
        }
        match &mut self.backend {
            Backend::Subprocess(c) => {
                c.set_harmony_source(chords, scales, transport);
                true
            }
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(_) => false,
        }
    }

    /// Install a per-block note-expression stream. `false` if the plugin
    /// declared no note-expression support.
    #[must_use = "a false return means the plugin declined this input and nothing was installed"]
    pub fn set_note_expression_source(
        &mut self,
        source: Arc<crate::host::node::NoteExpressionSource>,
    ) -> bool {
        if !self.accepts(Features::NOTE_EXPRESSION) {
            return false;
        }
        match &mut self.backend {
            Backend::Subprocess(c) => {
                c.set_note_expression_source(source);
                true
            }
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(_) => false,
        }
    }

    /// Install sample-accurate parameter automation — one curve per parameter id.
    ///
    /// Returns `()`, not `bool`, and takes no capability gate: every format
    /// carries parameter automation, so `PluginInputs` leaves this slot
    /// ungated. There is no "declined" answer to report.
    pub fn set_param_automation_source(
        &mut self,
        params: impl IntoIterator<Item = crate::host::node::TimedParam>,
        transport: impl tutti_core::transport::TransportState + 'static,
    ) {
        match &mut self.backend {
            Backend::Subprocess(c) => c.set_param_automation_source(params, transport),
            // The in-process VST2 node has no automation slot; its parameters
            // are driven through the handle instead.
            #[cfg(feature = "vst2")]
            Backend::InProcessVst2(_) => {}
        }
    }

    // ---- The graph node ---------------------------------------------------

    /// The node as the concrete out-of-process [`PluginClient`], unbound, for
    /// a host that wants the typed client: its
    /// [`controls`](PluginClient::controls) before insertion, or its own
    /// insertion path ([`bind`](PluginClient::bind) then `Editor::insert`,
    /// which hands back its controls and the fork source that forks it by
    /// state transfer).
    ///
    /// `Err` carries the node for a plugin that is not a `PluginClient` — an
    /// in-process VST2 instance, an `AudioUnit` that goes in through
    /// `tutti_graph::Legacy` and has no fork source yet; a fork of a graph
    /// holding it is refused as not forkable. Clone the
    /// [`handle`](Self::handle) first if the control surface is needed after
    /// this.
    ///
    /// Boxed, like the backend holding it: a `PluginClient` is several KiB.
    pub fn into_client(
        self,
    ) -> std::result::Result<Box<PluginClient>, Box<dyn tutti_core::AudioUnit>> {
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
                let parts = tutti_graph::IntoNode::into_parts(tutti_graph::Legacy::new(*c));
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

        // Unprobed stays open — the AU and in-process-VST2 loaders both relied
        // on this before they were taught to report what they deliver.
        assert!(!is_declined(&LoadedPlugin::default(), Features::TRANSPORT));
    }
}
