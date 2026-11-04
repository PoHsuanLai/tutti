//! CLAP plugin loader - thin wrapper around clap-host crate.

use std::path::Path;
use tutti_plugin::server::{
    BusChannels, BusTopologies, ClapFeature, ContentScale, EditorPresence, EditorSize, Features,
    LoadedPlugin, Normalized, NoteExpressionChanges, ParamAddress, ParamRange, ParameterInfo,
    PluginAudio, PluginClass, PluginDescriptor, PluginEditorHost, PluginError, PluginMeta,
    PluginParams, PluginPresets, PluginResult, PluginState, PluginTail, PresetId, Samples,
    WindowHandle,
};
use tutti_plugin::server::{ProcessContext, ProcessOutput, RenderMode};

use crate::loaders::common::{single_bus, Meta};
use tutti_plugin::{BridgeError, LoadStage, Result};

/// Build the catalog descriptor from CLAP factory info, carrying its feature
/// tags verbatim as the native class.
#[cfg(feature = "clap")]
/// Per-bus channel counts for one direction, main bus first, read off the
/// CLAP `audio-ports` extension (e.g. `[2, 1]` = stereo main + mono sidechain).
/// Falls back to a single aggregate main bus when the plugin doesn't implement
/// the extension (empty port list), matching the single-bus legacy convention
/// the downstream slab expects.
fn per_bus_channels(loaded: &tutti_clap_host::ClapLoaded, is_input: bool) -> BusChannels {
    let count = loaded.audio_port_count(is_input);
    let buses: BusChannels = (0..count)
        .filter_map(|i| loaded.audio_port_info(i, is_input))
        .map(|p| p.layout)
        .collect();
    if buses.is_empty() {
        // No `audio-ports` extension: fall back to the aggregate total the
        // host reports, as a single main bus.
        let total = if is_input {
            loaded.info().audio_inputs
        } else {
            loaded.info().audio_outputs
        };
        single_bus(total)
    } else {
        buses
    }
}

/// The pool this process serves `clap.thread-pool` requests on, built the
/// first time a plugin that implements the extension loads; `None` on a
/// machine too small for one (below).
///
/// The engine's own pool lives in the host process, and a plugin's `process`
/// runs here, in another: a request cannot reach it. So the server keeps a
/// `WorkerPool` of its own, with its helpers raised to the audio thread's
/// realtime priority, as `tutti_core::WorkerPool` requires of helpers. A
/// plugin that does not implement the extension costs no threads.
///
/// Sized by [`pool_participants`]: half the cores, at most 4, the server's
/// audio thread included. Each plugin instance has a server process of its
/// own, so every pooled instance adds a pool, and the engine's pool in the
/// host wants the same cores; half leaves the other half to them, and the cap
/// keeps a few pooled instances from oversubscribing a large machine, where
/// one block's tasks rarely keep more than a few threads busy anyway. Below
/// two participants (fewer than 4 cores) no pool is built, and requests are
/// refused: the plugin's own fallback does as well as a pool of one.
///
/// A request is one span of one block, so its helpers spin only briefly and
/// park soon after (10 µs, then 50 µs of yielding) rather than holding cores
/// the engine could use between blocks. That costs a request nothing: it
/// never waits for a helper that has not started a task (the audio thread
/// runs tasks from the start, and helpers join as they arrive), though a
/// helper preempted mid-task delays the request.
#[cfg(feature = "clap")]
fn thread_pool() -> Option<std::sync::Arc<dyn tutti_core::Pool>> {
    THREAD_POOL
        .get_or_init(|| {
            let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
            let participants = pool_participants(cores);
            (participants >= 2).then(|| {
                std::sync::Arc::new(
                    tutti_core::WorkerPool::builder(participants)
                        .name("tutti-clap-pool")
                        .spin(std::time::Duration::from_micros(10))
                        .idle(std::time::Duration::from_micros(50))
                        .on_start(std::sync::Arc::new(|_| {
                            crate::server::raise_to_realtime("a clap.thread-pool helper")
                        }))
                        .build(),
                )
            })
        })
        .clone()
        .map(|pool| pool as std::sync::Arc<dyn tutti_core::Pool>)
}

/// Participants of the server's pool on a machine with `cores` cores (see
/// [`thread_pool`]): half of them, at most 4.
#[cfg(feature = "clap")]
fn pool_participants(cores: usize) -> usize {
    (cores / 2).min(4)
}

/// [`thread_pool`]'s pool, once decided.
#[cfg(feature = "clap")]
static THREAD_POOL: std::sync::OnceLock<Option<std::sync::Arc<tutti_core::WorkerPool>>> =
    std::sync::OnceLock::new();

/// What the two editor answers mean for the three capability bits.
struct EditorBits {
    /// `Features::EDITOR` — is there a UI at all?
    has_editor: bool,
    /// Whether `Features::EDITOR_RESIZE` may be set, subject to the plugin's
    /// own resize hints.
    resize_allowed: bool,
    /// `Features::EDITOR_FLOATING` — does the plugin own the window?
    floating: bool,
}

/// Resolve the two editor answers into the three bits that depend on them.
///
/// CLAP asks about editors twice — once for embedded, once for floating — and
/// each bit wants a different combination:
///
/// - **`Features::EDITOR` is the union.** It means "this plugin has a UI", and
///   a floating-only plugin has one. Reporting only the embedded answer is what
///   made such a plugin look editor-less, so the DAW drew no button for a UI it
///   could have shown.
/// - **`Features::EDITOR_RESIZE` is embedded-only.** `can_resize`,
///   `adjust_size` and `set_size` are all marked `[main-thread & !floating]` in
///   `ext/gui.h`; a floating window is the plugin's to size. Advertising a
///   resize path for one would announce a capability nothing can drive.
/// - **`Features::EDITOR_FLOATING` is "embedding is not available".** Set only
///   when the plugin floats *and cannot embed* — a plugin supporting both is
///   hosted embedded, because that gives the host control over placement and
///   keeps the editor inside the session's window management. Floating is the
///   fallback for plugins with no choice, not a preference to honour.
///
/// That last rule is why `prefers_floating()` is not consulted here.
/// `ext/gui.h:113` calls the preference a hint the host has no obligation to
/// honour, and a host that floated every plugin asking to would scatter windows
/// it can no longer place, size or stack.
///
/// A free function because these rules are the finding, and reaching them
/// through the loader needs a live plugin behind a real dlopen — see the tests
/// below, which pin the combinations without one.
fn editor_bits(embeddable: bool, floating: bool) -> EditorBits {
    EditorBits {
        has_editor: embeddable || floating,
        resize_allowed: embeddable,
        floating: floating && !embeddable,
    }
}

fn clap_descriptor(info: &tutti_clap_host::PluginInfo, editor: EditorPresence) -> PluginDescriptor {
    PluginDescriptor {
        id: info.id.clone(),
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        version: info.version.clone(),
        class: PluginClass::Clap {
            features: info
                .features
                .iter()
                .map(|f| ClapFeature::parse(f))
                .collect(),
        },
        editor,
    }
}

#[cfg(feature = "clap")]
use tutti_clap_host::{ClapActive, ClapLoaded};

/// Active host instance, monomorphised by sample format at activation time:
/// `F64` only when the session asked for 64-bit and every port of the plugin
/// advertises it (see [`ClapInstance::load`]).
#[cfg(feature = "clap")]
enum ClapInner {
    F32(ClapActive<f32>),
    F64(ClapActive<f64>),
}

/// Dispatch a shared expression over both inner variants (immutable). The bound
/// methods come from `ClapLoaded` via `Deref`, so they resolve on either arm.
#[cfg(feature = "clap")]
macro_rules! clap_dispatch {
    ($self:expr, $inner:ident => $body:expr) => {
        match &$self.inner {
            ClapInner::F32($inner) => $body,
            ClapInner::F64($inner) => $body,
        }
    };
}

/// Dispatch a shared expression over both inner variants (mutable).
#[cfg(feature = "clap")]
macro_rules! clap_dispatch_mut {
    ($self:expr, $inner:ident => $body:expr) => {
        match &mut $self.inner {
            ClapInner::F32($inner) => $body,
            ClapInner::F64($inner) => $body,
        }
    };
}

/// A plugin that is open and not yet active: what [`ClapInstance::open`]
/// reads before activation.
#[cfg(feature = "clap")]
struct Opened {
    loaded: ClapLoaded,
    descriptor: PluginDescriptor,
    features: Features,
}

pub struct ClapInstance {
    #[cfg(feature = "clap")]
    inner: ClapInner,
    meta: Meta,
    /// The layout (or the instance's ability to run) changed since the host
    /// last heard; see [`take_io_changed`](Self::take_io_changed).
    #[cfg(feature = "clap")]
    io_changed: bool,
    /// What a re-activation or a rescan moved that the host has not heard of
    /// yet; see [`take_changes`](Self::take_changes).
    #[cfg(feature = "clap")]
    pending: Pending,
}

/// What the plugin moved since the host last heard, each reported once.
#[cfg(feature = "clap")]
#[derive(Default)]
struct Pending {
    latency: Option<Samples>,
    tail: Option<PluginTail>,
    param_values: bool,
    param_titles: bool,
}

/// What [`ClapInstance::take_changes`] hands the poll.
#[cfg(feature = "clap")]
#[derive(Debug, Default, PartialEq)]
pub struct ClapChanges {
    /// The latency, when it moved. Read from the plugin while it was
    /// activated, which is the only time CLAP lets it change.
    pub latency: Option<Samples>,
    /// The tail, when it moved.
    pub tail: Option<PluginTail>,
    /// Parameter values (or their display text) changed.
    pub param_values: bool,
    /// Parameters were added, removed or described differently.
    pub param_titles: bool,
}

/// What the plugin declares on its audio ports: the buses and, per port,
/// what `clap.surround` says (`None` for a port the plugin will not answer
/// for: no extension, or no `get_channel_map`).
#[cfg(feature = "clap")]
struct BusLayout {
    inputs: BusChannels,
    outputs: BusChannels,
    input_topology: BusTopologies,
    output_topology: BusTopologies,
}

#[cfg(feature = "clap")]
impl BusLayout {
    fn read(loaded: &ClapLoaded) -> Self {
        let inputs = per_bus_channels(loaded, true);
        let outputs = per_bus_channels(loaded, false);
        let topology = |is_input: bool, count: usize| {
            (0..count)
                .map(|i| loaded.surround_topology(is_input, i as u32))
                .collect()
        };
        Self {
            input_topology: topology(true, inputs.len()),
            output_topology: topology(false, outputs.len()),
            inputs,
            outputs,
        }
    }
}

// Safety: the inner host instances are Send.
unsafe impl Send for ClapInstance {}

impl ClapInstance {
    /// Lightweight probe: read CLAP descriptor without calling init() or activate().
    pub fn probe(path: &Path) -> Result<PluginDescriptor> {
        #[cfg(feature = "clap")]
        {
            let resolved = tutti_plugin::server::resolve_bundle(path)?;
            let library_path = if resolved != path {
                Some(resolved.as_path())
            } else {
                None
            };
            let info =
                ClapLoaded::probe(path, library_path).map_err(|e| BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Scanning,
                    reason: e.to_string(),
                })?;

            // A probe does not instantiate; the load path below asks.
            Ok(clap_descriptor(&info, EditorPresence::Unknown))
        }
        #[cfg(not(feature = "clap"))]
        Err(BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: "CLAP support not compiled".to_string(),
        })
    }

    /// Load and activate a CLAP plugin.
    ///
    /// If `prefer_f64` is `true` and the plugin advertises 64-bit support on
    /// every port at activation, the instance is activated as
    /// `ClapActive<f64>`; otherwise `f32`. The chosen format is what
    /// `metadata().features` reports as [`Features::F64_AUDIO`], so the
    /// session's negotiated format and the activation agree: the session sends
    /// the buffer width it negotiated, and an instance activated in the other
    /// width would refuse every block.
    ///
    /// Activation re-reads the plugin's audio ports, so the buses and the
    /// 64-bit answer in the metadata are the ones the plugin has when it is
    /// activated, not the ones it had when it was opened.
    pub fn load(
        path: &Path,
        sample_rate: f64,
        block_size: usize,
        prefer_f64: bool,
    ) -> Result<Self> {
        #[cfg(feature = "clap")]
        {
            Self::activate(Self::open(path, sample_rate, block_size)?, path, prefer_f64)
        }

        #[cfg(not(feature = "clap"))]
        {
            let _ = (path, sample_rate, block_size, prefer_f64);
            Err(BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Opening,
                reason: "CLAP support not compiled (enable 'clap' feature)".to_string(),
            })
        }
    }

    /// Opens the plugin, without activating it: the instance, its descriptor
    /// and the features that do not depend on its audio ports.
    #[cfg(feature = "clap")]
    fn open(path: &Path, sample_rate: f64, block_size: usize) -> Result<Opened> {
        let resolved = tutti_plugin::server::resolve_bundle(path)?;
        let library_path = if resolved != path {
            Some(resolved.as_path())
        } else {
            None
        };
        let mut loaded =
            ClapLoaded::load_with_library(path, library_path, sample_rate, block_size as u32)
                .map_err(|e| BridgeError::LoadFailed {
                    path: path.to_path_buf(),
                    // Host and bridge LoadStage are now the same shared type
                    // (tutti-plugin-types); pass the stage through unchanged.
                    stage: match e {
                        tutti_clap_host::ClapError::LoadFailed { stage, .. } => stage,
                        _ => LoadStage::Opening,
                    },
                    reason: e.to_string(),
                })?;

        if loaded.supports_thread_pool() {
            loaded.set_thread_pool(thread_pool());
        }

        // Two questions, not one — see [`editor_bits`].
        let embeddable = loaded.has_editor();
        let EditorBits {
            has_editor,
            resize_allowed,
            floating,
        } = editor_bits(embeddable, loaded.has_floating_editor());
        let editor_resizable = resize_allowed && loaded.editor_capabilities().resize.resizable;
        let has_note_in = loaded.note_port_count(true) > 0;
        let has_note_out = loaded.note_port_count(false) > 0;
        let descriptor = clap_descriptor(loaded.info(), EditorPresence::measured(has_editor));

        let mut features = Features::empty();
        features.set(Features::MIDI_IN, has_note_in);
        features.set(Features::MIDI_OUT, has_note_out);
        features.set(Features::EDITOR, has_editor);
        features.set(Features::EDITOR_RESIZE, editor_resizable);
        features.set(Features::EDITOR_FLOATING, floating);
        // CLAP always carries transport, sample-accurate param automation, and
        // the full note-expression dimension set (see build_clap_transport /
        // PARAM_VALUE events / CLAP_EVENT_NOTE_EXPRESSION). No sequencer context
        // (the CLAP spec defines no chord/scale events).
        features.insert(Features::TRANSPORT);
        features.insert(Features::PARAM_AUTOMATION);
        features.set(Features::NOTE_EXPRESSION, has_note_in);
        // `PRESET_LIST` stays out of `probed::CLAP` — enumeration lives in
        // the preset-discovery extension, which is factory-level and
        // unbound here — so only the load half is answered.
        features.set(Features::PRESET_LOAD, loaded.supports_preset_load());

        Ok(Opened {
            loaded,
            descriptor,
            features,
        })
    }

    /// Activates an opened plugin in the format the session will process, and
    /// builds the metadata from the plugin as activated.
    #[cfg(feature = "clap")]
    fn activate(opened: Opened, path: &Path, prefer_f64: bool) -> Result<Self> {
        let Opened {
            loaded,
            descriptor,
            features,
        } = opened;
        let activation_failed = |e: tutti_clap_host::ClapError| BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Activation,
            reason: format!("activate failed: {e}"),
        };

        // 64-bit only when it was asked for: the session's wire format
        // follows its preference (f32 by default), not the plugin's
        // capability. Activation re-reads the 64-bit flag, so a plugin that
        // dropped it since it was opened is refused here, and 32-bit is the
        // clean fallback (the refused instance comes back untouched).
        let inner = if prefer_f64 {
            match loaded.activate::<f64>() {
                Ok(active) => ClapInner::F64(active),
                Err((loaded, tutti_clap_host::ClapError::NotSupported(_))) => ClapInner::F32(
                    loaded
                        .activate::<f32>()
                        .map_err(|(_, e)| activation_failed(e))?,
                ),
                Err((_, e)) => return Err(activation_failed(e)),
            }
        } else {
            ClapInner::F32(
                loaded
                    .activate::<f32>()
                    .map_err(|(_, e)| activation_failed(e))?,
            )
        };

        // Everything below is read off the activated instance, so it is what
        // the plugin declares now.
        let active_loaded: &ClapLoaded = match &inner {
            ClapInner::F32(i) => i,
            ClapInner::F64(i) => i,
        };

        let layout = BusLayout::read(active_loaded);

        let mut features = features;
        // The format actually activated, as VST3 reports it.
        features.set(Features::F64_AUDIO, matches!(inner, ClapInner::F64(_)));

        let loaded_meta = LoadedPlugin {
            inputs: layout.inputs,
            outputs: layout.outputs,
            input_topology: layout.input_topology,
            output_topology: layout.output_topology,
            // Latency and tail are queryable after activation. A plugin
            // without `clap.tail` has not answered: its tail is unknown,
            // which is not the `None` a plugin answering 0 gives.
            latency_samples: Samples(active_loaded.get_latency() as usize),
            tail: active_loaded
                .get_tail()
                .map_or(PluginTail::Unknown, PluginTail::from_clap_samples),
            features,
            probed: tutti_plugin::server::probed::CLAP,
        };

        Ok(Self {
            inner,
            meta: Meta {
                descriptor,
                loaded: loaded_meta,
            },
            io_changed: false,
            pending: Pending::default(),
        })
    }

    /// Re-reads the bus layout after the plugin was reconfigured, refreshing
    /// the metadata and raising [`take_io_changed`](Self::take_io_changed)
    /// when it differs from what the host was last given. A plugin may change
    /// it while deactivated, which a rate change does.
    #[cfg(feature = "clap")]
    fn note_layout_change(&mut self) {
        let now = BusLayout::read(clap_dispatch!(self, i => i));
        let meta = &mut self.meta.loaded;
        if now.inputs != meta.inputs
            || now.outputs != meta.outputs
            || now.input_topology != meta.input_topology
            || now.output_topology != meta.output_topology
        {
            meta.inputs = now.inputs;
            meta.outputs = now.outputs;
            meta.input_topology = now.input_topology;
            meta.output_topology = now.output_topology;
            self.io_changed = true;
        }
    }

    /// Acts on what the plugin asked of the host since the last call.
    ///
    /// Its main-thread work runs first, since it may ask for the rest: the
    /// `on_main_thread` a `request_callback` wanted, the timers that are due and
    /// the file descriptors that are ready. Then, when the plugin asked for a
    /// restart (`request_restart`, an `audio-ports.rescan` or a
    /// `params.rescan(ALL)` that needs the plugin deactivated, or a
    /// `latency.changed` while it is active, which CLAP allows only across an
    /// activation), the one way CLAP has: deactivate, re-read the ports,
    /// activate again. Latency, tail and the layout are read after it, while
    /// the plugin is activated, and the host is told what moved. A
    /// `params.rescan` that changed what the parameters are re-reads their
    /// ranges.
    ///
    /// Timers and callbacks are serviced only while the host keeps calling
    /// this: a loop that blocks in `recv` for the next request services none
    /// until one arrives.
    ///
    /// Call it between blocks, on the thread that loaded the plugin
    /// (`deactivate` and `activate` are `[main-thread]` and never overlap a
    /// `process`). It allocates, as a rate change does. A request is consumed
    /// whether or not the plugin can be brought back: a plugin left
    /// deactivated renders silence and raises [`take_io_changed`], and a later
    /// request tries it again.
    ///
    /// [`take_io_changed`]: Self::take_io_changed
    #[cfg(feature = "clap")]
    pub fn service_requests(&mut self) {
        clap_dispatch_mut!(self, i => i.run_main_thread_work());
        let (ports, restart, params, latency, tail) = clap_dispatch!(self, i => (
            i.poll_audio_ports_rescan(),
            i.poll_restart_requested(),
            i.poll_params_rescan(),
            i.poll_latency_changed(),
            i.poll_tail_changed(),
        ));
        let was_active = clap_dispatch!(self, i => i.is_active());
        let params_before = clap_dispatch!(self, i => i.parameter_count());

        // A successful re-activation re-reads the parameters' ranges itself.
        let reactivate = ports.needs_deactivate()
            || restart
            || params.needs_deactivate()
            || (latency && was_active);
        if reactivate {
            let outcome = clap_dispatch_mut!(self, i => i.reactivate());
            let active = clap_dispatch!(self, i => i.is_active());
            self.note_layout_change();
            if let Err(e) = outcome {
                tracing::warn!("CLAP plugin could not be re-activated on its request: {e}");
            }
            // A plugin left deactivated, or brought back after a failed rate
            // change, is a different instance to the host than the one it last
            // read, whatever its layout.
            if active != was_active {
                self.io_changed = true;
            }
            // The re-activation re-read the parameters' ranges: tell the host
            // when the list is not the one it holds.
            if clap_dispatch!(self, i => i.parameter_count()) != params_before {
                self.pending.param_titles = true;
            }
        }
        // Latency is read with the plugin activated (`clap.latency` allows it
        // to change only across an activation), so a `changed()` on a
        // deactivated plugin waits for the next one.
        if reactivate {
            // A `latency.changed` is reported as it comes, the figure being
            // the one read after the activation.
            self.note_latency_change(latency);
        }
        if reactivate || tail {
            // A `tail.changed` is reported as it comes; a re-read after an
            // activation, only when the figure moved.
            self.note_tail_change(tail);
        }

        if params.requested {
            if params.info || params.all {
                // Read after the activation: a plugin may settle its
                // parameters in `activate`.
                clap_dispatch_mut!(self, i => i.rescan_params());
                self.pending.param_titles = true;
            }
            if params.values || params.text {
                self.pending.param_values = true;
            }
        }
    }

    /// Reads the latency from the activated plugin and keeps it when it moved
    /// (or when `announced`: the plugin said it changed). Nothing happens while
    /// the plugin is deactivated: `get` is allowed only when it is (being)
    /// activated.
    #[cfg(feature = "clap")]
    fn note_latency_change(&mut self, announced: bool) {
        if !clap_dispatch!(self, i => i.is_active()) {
            return;
        }
        let latency = Samples(clap_dispatch!(self, i => i.get_latency()) as usize);
        if announced || latency != self.meta.loaded.latency_samples {
            self.meta.loaded.latency_samples = latency;
            self.pending.latency = Some(latency);
        }
    }

    /// The tail twin of [`note_latency_change`](Self::note_latency_change).
    /// `announced` is a `tail.changed` the plugin made: it is reported even
    /// when the figure is the one the host holds.
    #[cfg(feature = "clap")]
    fn note_tail_change(&mut self, announced: bool) {
        let tail = clap_dispatch!(self, i => i.get_tail())
            .map_or(PluginTail::Unknown, PluginTail::from_clap_samples);
        if announced || tail != self.meta.loaded.tail {
            self.meta.loaded.tail = tail;
            self.pending.tail = Some(tail);
        }
    }

    /// What the last [`service_requests`](Self::service_requests) or sample-rate
    /// change moved, besides the layout ([`take_io_changed`](Self::take_io_changed)).
    /// Clears it.
    #[cfg(feature = "clap")]
    pub fn take_changes(&mut self) -> ClapChanges {
        let pending = std::mem::take(&mut self.pending);
        ClapChanges {
            latency: pending.latency,
            tail: pending.tail,
            param_values: pending.param_values,
            param_titles: pending.param_titles,
        }
    }

    /// Whether the bus layout, the 64-bit answer or the instance's ability to
    /// run changed since the last call, which clears it. The host re-reads the
    /// instance on the [`IoChanged`](crate::plugin::AsyncEvent::IoChanged) it
    /// becomes.
    #[cfg(feature = "clap")]
    pub fn take_io_changed(&mut self) -> bool {
        std::mem::take(&mut self.io_changed)
    }

    /// Test-only access to the underlying loaded instance (via `Deref` through
    /// whichever active arm). Lets the integration tests exercise the read-only
    /// `ClapLoaded` surface (poll_*, port/note queries, state context, …)
    /// without threading the f32/f64 enum through every assertion.
    #[cfg(all(feature = "clap", test))]
    pub(crate) fn clap_loaded(&self) -> &tutti_clap_host::ClapLoaded {
        clap_dispatch!(self, i => &**i)
    }

    /// Test-only mutable access (for `poll_timers`, `on_main_thread`, …).
    #[cfg(all(feature = "clap", test))]
    fn loaded_mut(&mut self) -> &mut tutti_clap_host::ClapLoaded {
        clap_dispatch_mut!(self, i => &mut **i)
    }

    /// Test-only: has the active instance started processing? (`is_processing`
    /// lives on `ClapActive`, so it is reached through the enum, not `Deref`.)
    #[cfg(all(feature = "clap", test))]
    fn is_processing(&self) -> bool {
        clap_dispatch!(self, i => i.is_processing())
    }

    /// Drive one block through a typed active instance. Free of `self.inner`
    /// so it works for whichever arm of [`ClapInner`] matches the buffer format.
    #[cfg(feature = "clap")]
    fn process_active<'a, T: tutti_clap_host::ClapSample>(
        active: &'a mut ClapActive<T>,
        clap_buffer: &mut tutti_clap_host::AudioBuffer<T>,
        ctx: &ProcessContext,
        sample_rate: f64,
    ) -> Result<tutti_clap_host::instance::ProcessOutputRef<'a>> {
        // Both sides of this hop are `Option<&ParameterChanges>`, so the borrow
        // passes straight through — cloning here would deep-copy the change list
        // on every block only to re-borrow it two lines later.
        let note_expressions: Vec<tutti_clap_host::ClapNoteExpression> = ctx
            .note_expression
            .map(convert_note_expressions)
            .unwrap_or_default();
        let transport = ctx.transport.map(|t| convert_transport(t, sample_rate));

        let clap_ctx = tutti_clap_host::ClapProcessContext {
            midi: ctx.midi_events,
            // An empty list means "no automation this block"; the plugin reads
            // `None` and `Some(empty)` differently, so the mapping is kept.
            params: ctx.param_changes.filter(|p| !p.is_empty()),
            expressions: &note_expressions,
            transport: transport.as_ref(),
        };

        active
            .process(clap_buffer, &clap_ctx)
            .map_err(|e| BridgeError::ProcessError(e.to_string()))
    }
}

#[cfg(feature = "clap")]
impl PluginMeta for ClapInstance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.meta.loaded
    }
}

impl PluginAudio for ClapInstance {
    fn process(
        &mut self,
        buffer: tutti_plugin::server::AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
        out: &mut ProcessOutput,
    ) -> PluginResult<()> {
        use tutti_plugin::server::AudioBufferMut;
        // A rate change that could not bring the plugin back left it
        // deactivated (the host raised `IoChanged` when it happened). CLAP
        // allows `process` only while active, so the block is silence and the
        // plugin is not touched.
        if !clap_dispatch!(self, i => i.is_active()) {
            out.clear();
            match buffer {
                AudioBufferMut::F32(mut buf) => buf.clear_outputs(),
                AudioBufferMut::F64(mut buf) => buf.clear_outputs(),
            }
            return Ok(());
        }
        // The buffer format must match the format the instance was activated
        // with (the `ClapInner` arm). A mismatch is a negotiation bug upstream;
        // surface it rather than silently mis-cast.
        match (buffer, &mut self.inner) {
            (AudioBufferMut::F32(buf), ClapInner::F32(active)) => {
                let sr = buf.sample_rate;
                let mut clap_buffer = tutti_clap_host::AudioBuffer32 {
                    inputs: buf.inputs,
                    outputs: buf.outputs,
                    num_samples: buf.num_samples,
                    sample_rate: sr,
                };
                copy_process_output(
                    Self::process_active(active, &mut clap_buffer, ctx, sr)?,
                    out,
                );
                Ok(())
            }
            (AudioBufferMut::F64(buf), ClapInner::F64(active)) => {
                let sr = buf.sample_rate;
                let mut clap_buffer = tutti_clap_host::AudioBuffer64 {
                    inputs: buf.inputs,
                    outputs: buf.outputs,
                    num_samples: buf.num_samples,
                    sample_rate: sr,
                };
                copy_process_output(
                    Self::process_active(active, &mut clap_buffer, ctx, sr)?,
                    out,
                );
                Ok(())
            }
            // The `.to_string()` here is not steady state and stays — see the
            // matching arm in the vst3 loader for the argument.
            (AudioBufferMut::F32(_), ClapInner::F64(_))
            | (AudioBufferMut::F64(_), ClapInner::F32(_)) => Err(PluginError::Process(
                "audio buffer sample format does not match the format the CLAP \
                 plugin was activated with"
                    .to_string(),
            )),
        }
    }

    /// Forward to `clap.render`, reporting whether the plugin took it.
    ///
    /// `clap_plugin_render.set` is `[main-thread]` (`ext/render.h:33`) and
    /// carries no activation constraint, so unlike VST3 and AU this needs no
    /// deactivate/reactivate bracket — the live instance accepts it.
    ///
    /// `false` here is a real answer, not a failure to ask: the extension is
    /// optional by design (*"If this information does not influence your
    /// rendering code, then don't implement this extension"*), so a plugin
    /// that does not export it has genuinely declined.
    fn set_render_mode(&mut self, mode: RenderMode) -> bool {
        let offline = mode.is_offline();
        clap_dispatch_mut!(self, i => i.set_render_mode(offline))
    }

    /// A refusal rolls the instance back to the rate the plugin already
    /// accepted and it stays active there. A plugin that cannot be brought
    /// back, or that drops the 64-bit audio its instance runs, is left
    /// deactivated: `process` renders silence and the host is told through
    /// [`take_io_changed`](Self::take_io_changed). That instance is not
    /// rebuilt as 32-bit, because the session's wire format was negotiated at
    /// load and a 32-bit instance would refuse every 64-bit block instead.
    ///
    /// The plugin may also change its bus layout across the re-activation;
    /// the metadata follows it and the host is told the same way.
    fn set_sample_rate(&mut self, rate: f64) -> PluginResult<()> {
        let outcome = clap_dispatch_mut!(self, i => {
            i.set_sample_rate(rate)
                .map_err(|e| (e, i.sample_rate(), i.is_active()))
        });
        self.note_layout_change();
        // The latency and tail may depend on the rate; they are read from the
        // plugin as it is now activated.
        self.note_latency_change(false);
        self.note_tail_change(false);
        outcome.map_err(|(e, running_at, active)| {
            if !active {
                // Nothing else will reach the host: the instance now renders
                // silence, so it re-reads this one.
                self.io_changed = true;
            }
            PluginError::Other(format!(
                "CLAP plugin refused sample rate {rate} (running at {running_at}, {}): {e}",
                if active { "active" } else { "deactivated" }
            ))
        })
    }
}

impl PluginParams for ClapInstance {
    /// Normalized `0..=1`, per the [`PluginParams`] contract — CLAP has no
    /// normalization concept, so the plugin answers in its plain range and the
    /// declared bounds map it back.
    ///
    /// A parameter the plugin declares no range for is passed through rather
    /// than scaled against invented bounds — the same choice
    /// `add_param_changes` makes for an id missing from its range map.
    fn get_parameter(&self, id: ParamAddress) -> f64 {
        // A VST2 index addresses nothing here; `clap_id` is opaque.
        let Some(id) = id.opaque() else { return 0.0 };
        let Some(plain) = clap_dispatch!(self, i => i.get_parameter(id.get())) else {
            return 0.0;
        };
        match clap_dispatch!(self, i => i.parameter_range(id.get())) {
            Some((min, max)) => ParamRange::Plain {
                min,
                max,
                default: min,
            }
            .to_normalized(plain),
            None => plain,
        }
    }

    /// Normalized `0..=1` in, matching [`get_parameter`](Self::get_parameter).
    /// CLAP events carry the plain value, so the declared range denormalizes it
    /// — the same conversion `add_param_changes` applies on the automation path.
    fn set_parameter(&mut self, id: ParamAddress, value: Normalized) {
        let Some(id) = id.opaque() else { return };
        let plain = match clap_dispatch!(self, i => i.parameter_range(id.get())) {
            Some((min, max)) => ParamRange::Plain {
                min,
                max,
                default: min,
            }
            .to_plain(value.get()),
            None => value.get(),
        };
        clap_dispatch_mut!(self, i => {
            i.set_parameter(id.get(), plain);
        });
    }

    /// The plugin's own display string, denormalized on the way in.
    ///
    /// `clap_plugin_params.value_to_text` takes a **plain** value in the
    /// parameter's declared range — CLAP has no normalization concept — so this
    /// applies the same range conversion
    /// [`set_parameter`](Self::set_parameter) does. A parameter the plugin
    /// declared no range for is passed through rather than scaled against
    /// invented bounds, matching the read/write pair above.
    fn parameter_text(&self, id: ParamAddress, value: Normalized) -> Option<String> {
        let id = id.opaque()?;
        let plain = match clap_dispatch!(self, i => i.parameter_range(id.get())) {
            Some((min, max)) => ParamRange::Plain {
                min,
                max,
                default: min,
            }
            .to_plain(value.get()),
            None => value.get(),
        };
        clap_dispatch!(self, i => i.value_to_text(id.get(), plain))
    }

    /// The inverse, re-normalized on the way out so the result round-trips into
    /// [`set_parameter`](Self::set_parameter).
    fn parameter_value_from_text(&self, id: ParamAddress, text: &str) -> Option<Normalized> {
        let id = id.opaque()?;
        let plain = clap_dispatch!(self, i => i.text_to_value(id.get(), text))?;
        let normalized = match clap_dispatch!(self, i => i.parameter_range(id.get())) {
            Some((min, max)) => ParamRange::Plain {
                min,
                max,
                default: min,
            }
            .to_normalized(plain),
            None => plain,
        };
        Some(Normalized::new(normalized))
    }

    fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        // The host crate projects CLAP-native param info onto the shared
        // `ParameterInfo` at its own boundary, so the loader maps no flags here.
        clap_dispatch!(self, i => i.get_parameter_list())
    }

    fn parameter_count(&self) -> usize {
        clap_dispatch!(self, i => i.parameter_count())
    }
}

impl PluginEditorHost for ClapInstance {
    fn open_editor(
        &mut self,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        // Safety: WindowHandle was validated at the IPC boundary in server.rs
        let handle = unsafe { tutti_clap_host::WindowHandle::from_raw(parent.as_ptr()) };
        clap_dispatch_mut!(self, i => i.open_editor(handle, scale))
            .map(|s| EditorSize {
                width: s.width,
                height: s.height,
            })
            .map_err(|e| PluginError::Editor(tutti_plugin::EditorError::PluginError(e.to_string())))
    }

    fn close_editor(&mut self) {
        clap_dispatch_mut!(self, i => {
            i.close_editor();
        });
    }
}

/// The filesystem path an id names, or `None` when it names none.
///
/// A free function so the decision is testable on its own. Through
/// `load_preset` it is not: a coerced non-path id produces a path that does not
/// exist, the plugin refuses *that*, and the caller sees the same `false` the
/// guard would have produced. The two refusals are indistinguishable from
/// outside, so only the guard's own inputs can pin it — verified by mutation.
///
/// Only [`PresetId::Location`] addresses a CLAP preset:
/// `clap_plugin_preset_load::from_location` takes a filesystem path and nothing
/// else. A `Number` or `Program` has no path to coerce *to*, which is why the
/// id is opaque rather than an integer.
fn clap_preset_path(id: &PresetId) -> Option<&std::path::Path> {
    match id {
        PresetId::Location(p) => Some(p.as_path()),
        PresetId::Number(_) | PresetId::Program { .. } => None,
    }
}

impl PluginPresets for ClapInstance {
    /// Load a preset from the path an id names, via `CLAP_EXT_PRESET_LOAD`.
    ///
    /// [`PresetId::Location`] is the only shape CLAP can address: the
    /// extension's `from_location` takes a filesystem path and nothing else.
    /// A `Number` or `Program` names no CLAP preset and is refused rather than
    /// coerced — there is no number to coerce it *to*, which is exactly why the
    /// id is opaque.
    #[cfg(feature = "clap")]
    fn load_preset(&mut self, id: &PresetId) -> bool {
        let Some(path) = clap_preset_path(id) else {
            return false;
        };
        clap_dispatch_mut!(self, i => i.load_preset(path).is_ok())
    }

    // `get_presets` and `get_current_preset` stay at their defaults — an empty
    // list and `None`.
    //
    // CLAP **cannot enumerate**. Discovery lives in the factory-level
    // preset-discovery extension, which is queried on the *factory* rather than
    // on a loaded instance and which this host does not bind, so there is no
    // instance-level call that could answer. `CLAP_EXT_PRESET_LOAD` answers
    // only "can this be pointed at a path".
    //
    // That is why an empty list here is not the same claim as an empty list
    // from AU: `Features::PRESET_LIST` is *unprobed* for CLAP (see
    // `probed::CLAP`), so a caller reads `None` — nobody asked — rather than
    // `Some(false)`, which would say the plugin declined.
}

impl PluginState for ClapInstance {
    fn get_state(&mut self) -> PluginResult<Vec<u8>> {
        clap_dispatch_mut!(self, i => i.get_state()).map_err(|e| PluginError::State(e.to_string()))
    }

    fn set_state(&mut self, data: &[u8]) -> PluginResult<()> {
        clap_dispatch_mut!(self, i => i.set_state(data))
            .map_err(|e| PluginError::State(e.to_string()))
    }
}

#[cfg(feature = "clap")]
fn convert_note_expressions(
    changes: &NoteExpressionChanges,
) -> Vec<tutti_clap_host::ClapNoteExpression> {
    // The expression dimension is the shared `NoteExpressionType` on both
    // sides; only CLAP's voice-addressed value wrapper differs (it adds
    // port/channel/key). `from_shared` fills those defaults in the host crate.
    changes
        .changes
        .iter()
        .map(tutti_clap_host::ClapNoteExpression::from_shared)
        .collect()
}

#[cfg(feature = "clap")]
fn convert_transport(
    transport: &tutti_plugin::server::TransportInfo,
    sample_rate: f64,
) -> tutti_clap_host::TransportInfo {
    let sr = if sample_rate.is_finite() && sample_rate > 0.0 {
        sample_rate
    } else {
        44100.0
    };
    // CLAP's `song_pos_seconds`. Prefer the wire's own seconds field (the
    // plugin node's project time, `transport_source::ProjectTime`); fall back
    // to the project-time sample position for a producer that reports only
    // that.
    let seconds = if transport.position.seconds != 0.0 {
        transport.position.seconds
    } else {
        transport.position.samples.unwrap_or(0) as f64 / sr
    };
    tutti_clap_host::TransportInfo::default()
        .with_playing(transport.state.playing)
        .with_recording(transport.state.recording)
        .with_tempo(transport.timing.tempo)
        .with_time_signature(transport.timing.signature)
        .with_position_beats(transport.position.quarters, seconds)
        .with_loop(
            transport.state.cycle_active,
            transport.loop_region.start_quarters,
            transport.loop_region.end_quarters,
        )
        // The bar number arrives over the wire; pass it through rather than
        // hardcoding 0, which would put every plugin at bar one.
        .with_bar(transport.bar.position_quarters, transport.bar.number)
}

/// Copy one block's emitted events out of the CLAP host's pooled buffers into
/// the caller's reusable [`ProcessOutput`].
///
/// `out` arrives cleared, with its heap capacity intact, so every copy and
/// `add_change` below writes into storage that already exists. This is the
/// point of taking `out` by reference: `ClapInstance::process` returns a
/// **borrowing** `ProcessOutputRef` into its own pools, and building a fresh
/// owned `ProcessOutput` here — as this did — threw that away and allocated
/// three collections per block on the audio thread.
#[cfg(feature = "clap")]
fn copy_process_output(
    output: tutti_clap_host::instance::ProcessOutputRef<'_>,
    out: &mut ProcessOutput,
) {
    out.emit_midi(output.midi_events.iter().copied());

    for queue in &output.param_changes.queues {
        for point in &queue.points {
            // Pooled queues — see the vst3 loader's `process_block` for why
            // this is not `ParameterChanges::add_change`.
            out.emit_param_point(queue.param_id, point.sample_offset, point.value.get());
        }
    }

    // CLAP's note expression uses the shared `NoteExpressionType` (Pressure /
    // Expression included), so this narrows CLAP's voice-addressed value down
    // to the protocol shape without losing the dimension.
    for expr in output.note_expressions {
        out.note_expression
            .add_change(tutti_plugin::server::NoteExpressionValue {
                sample_offset: expr.sample_offset,
                note_id: expr.note_id,
                expression_type: expr.expression_type,
                value: expr.value,
            });
    }
}

#[cfg(test)]
#[cfg(feature = "clap")]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::Ordering;
    use tutti_midi_types::{MidiChannel, MidiGroup};
    use tutti_plugin::server::{
        NoteExpressionChanges, NoteExpressionType, NoteExpressionValue, ParameterChanges,
        ParameterQueue, TransportInfo,
    };

    /// The reference CLAP plugin, built as a dev-dependency by this same
    /// `cargo test` run. Resolved rather than hard-coded so the suite runs on
    /// any machine.
    fn clap_plugin() -> &'static str {
        crate::test_utils::clap_probe_path()
    }

    /// A floating-only plugin reports an editor.
    ///
    /// The user-visible half of the floating-window gap: `Features::EDITOR`
    /// drives whether the DAW offers to open a UI at all, and keying it on the
    /// embedded answer alone hid every floating-only plugin's editor behind a
    /// button that was never drawn.
    #[test]
    fn a_floating_only_plugin_reports_an_editor() {
        assert!(
            editor_bits(false, true).has_editor,
            "floating-only means the plugin has a UI, just not an embeddable one"
        );
    }

    /// ...but does not advertise resize.
    ///
    /// The pair that keeps the union from being applied to both bits. Every
    /// resize entry point is `[!floating]`, so a host acting on this bit for a
    /// floating window would be calling what the spec forbids.
    #[test]
    fn a_floating_only_plugin_does_not_advertise_resize() {
        assert!(
            !editor_bits(false, true).resize_allowed,
            "can_resize/adjust_size/set_size are all !floating — a floating \
             window is the plugin's to size"
        );
    }

    /// Floating is reported only when embedding is *unavailable*.
    ///
    /// The rule that keeps the host in control of its own window management: a
    /// plugin supporting both is embedded, so the session can place, size and
    /// stack it. Reading `EDITOR_FLOATING` as "the plugin would like to float"
    /// instead would scatter windows the host can no longer manage — and
    /// `ext/gui.h:113` explicitly calls the preference a hint the host need not
    /// honour.
    #[test]
    fn floating_is_reported_only_when_embedding_is_unavailable() {
        assert!(
            editor_bits(false, true).floating,
            "floating-only: nothing else is possible, so the host must float it"
        );
        assert!(
            !editor_bits(true, true).floating,
            "supports both: embed it, because that is the mode the host can manage"
        );
        assert!(
            !editor_bits(true, false).floating,
            "embed-only is not floating"
        );
        assert!(
            !editor_bits(false, false).floating,
            "no editor at all is not a floating editor"
        );
    }

    /// An embeddable plugin gets both bits, and a plugin with neither gets none.
    ///
    /// The two ends. Without the second, `editor_bits` returning `(true, true)`
    /// unconditionally would satisfy every other case here.
    #[test]
    fn editor_bits_track_the_plugin() {
        let embed_only = editor_bits(true, false);
        assert!(embed_only.has_editor && embed_only.resize_allowed);

        let both = editor_bits(true, true);
        assert!(
            both.has_editor && both.resize_allowed,
            "supporting both is still embeddable, so resize stays available"
        );

        let neither = editor_bits(false, false);
        assert!(
            !neither.has_editor && !neither.resize_allowed,
            "no editor either way means no editor — this is the case \
             `Features::EDITOR` must still be able to report"
        );
    }

    #[test]
    fn test_clap_metadata() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let instance =
            ClapInstance::load(path, 44100.0, 512, false).expect("Failed to load CLAP plugin");
        let meta = instance.descriptor();
        assert!(!meta.name.is_empty(), "Plugin name should not be empty");
        assert!(!meta.id.is_empty(), "Plugin id should not be empty");

        let loaded = instance.loaded();

        let inputs = loaded.total_inputs();
        let outputs = loaded.total_outputs();
        assert!(inputs > 0, "Expected audio inputs > 0, got {inputs}");
        assert!(outputs > 0, "Expected audio outputs > 0, got {outputs}");
        // `F64_AUDIO` is the format activated, and this load asked for f32.
        // The session suite covers the 64-bit cases
        // (`an_f64_session_runs_a_64bit_capable_clap_plugin_in_f64` and its
        // neighbours).
        assert!(!instance.loaded().features.contains(Features::F64_AUDIO));
    }

    /// **A plugin without `clap.tail` has an unknown tail, not none.** The
    /// extension's absence read as the `0` of "no tail", so a bounce sized
    /// from it cut off every such plugin's decay. A plugin that does answer
    /// keeps its figure.
    ///
    /// Mutation (run): in `ClapInstance::activate`, change
    /// `.map_or(PluginTail::Unknown, PluginTail::from_clap_samples)` to
    /// `.map_or(PluginTail::None, PluginTail::from_clap_samples)` → the first
    /// assertion sees `None` → fails.
    #[test]
    fn a_plugin_without_the_tail_extension_has_an_unknown_tail() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        {
            let _without = crate::test_utils::clap_probe_without_tail();
            let instance = ClapInstance::load(path, 44100.0, 512, false).expect("load");
            assert_eq!(instance.loaded().tail, PluginTail::Unknown);
        }
        let instance = ClapInstance::load(path, 44100.0, 512, false).expect("load");
        assert_eq!(
            instance.loaded().tail,
            PluginTail::Finite(Samples(
                tutti_clap_test_plugin::REPORTED_TAIL_SAMPLES as usize
            ))
        );
    }

    /// **The metadata follows the plugin's ports at activation, not at open.**
    /// The buses were read before `activate` and never refreshed, so a plugin
    /// that changed its layout in between was described by the old one.
    ///
    /// Mutation (run): read the input buses in `ClapInstance::open`
    /// (`per_bus_channels(&loaded, true)`), carry them in `Opened`, and use
    /// them in `ClapInstance::activate` → the one-port layout of the open →
    /// the assertions fail.
    #[test]
    fn the_buses_are_those_the_plugin_has_at_activation() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let opened = ClapInstance::open(path, 44100.0, 512).expect("open");

        let _layout = crate::test_utils::clap_probe_port_layout(
            tutti_clap_test_plugin::PortLayoutMode::AsymmetricAux as u32,
        );
        let instance = ClapInstance::activate(opened, path, false).expect("activate");

        let loaded = instance.loaded();
        assert_eq!(loaded.inputs.len(), 2, "stereo main + mono aux");
        assert_eq!(loaded.total_inputs(), 3);
        assert_eq!(loaded.total_outputs(), 3);
    }

    /// **A plugin that withdraws its 64-bit flags after it was opened loads as
    /// 32-bit.** The session asked for 64-bit on the answer the plugin gave at
    /// open; activation asks again, and the refusal is not a failed load.
    ///
    /// Mutation (run): in `ClapInstance::activate`, turn the
    /// `ClapError::NotSupported` fallback arm into `return Err(..)` → the load
    /// fails → the `expect` fails.
    #[test]
    fn a_plugin_that_drops_its_64_bit_flags_after_open_loads_as_32_bit() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let _flagged = crate::test_utils::clap_probe_ports_64bit(both);
        let opened = ClapInstance::open(path, 44100.0, 512).expect("open");
        assert!(opened.loaded.supports_f64(), "fixture: flagged at open");

        let _dropped = crate::test_utils::clap_probe_ports_64bit(0);
        let instance =
            ClapInstance::activate(opened, path, true).expect("falls back to 32-bit audio");
        assert!(!instance.loaded().features.contains(Features::F64_AUDIO));
        assert!(matches!(instance.inner, ClapInner::F32(_)));
    }

    /// **A rate change that changes the plugin's bus layout tells the host
    /// and refreshes the metadata.** Only VST3 raised `IoChanged`; a CLAP
    /// plugin that re-declared its ports across a re-activation left the host
    /// with the layout it was loaded with.
    ///
    /// Mutations (run): delete `self.note_layout_change();` in
    /// `ClapInstance::set_sample_rate` → the first assertion fails. Delete the
    /// `if clap.take_io_changed()` block in `Plugin::poll_async_events` → the
    /// polled-event assertion fails. Make
    /// `note_layout_change` compare nothing → the first assertion fails.
    #[test]
    fn a_rate_change_that_changes_the_layout_raises_io_changed() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance = ClapInstance::load(path, 44100.0, 512, false).expect("load");
        assert_eq!(instance.loaded().inputs.len(), 1, "fixture: one input bus");
        assert!(!instance.take_io_changed(), "nothing changed at load");

        let _layout = crate::test_utils::clap_probe_port_layout(
            tutti_clap_test_plugin::PortLayoutMode::AsymmetricAux as u32,
        );
        instance.set_sample_rate(48000.0).expect("accepted");

        assert_eq!(instance.loaded().inputs.len(), 2, "stereo main + mono aux");
        assert_eq!(instance.loaded().total_inputs(), 3);
        let mut plugin = crate::plugin::Plugin::Clap(instance);
        assert!(
            plugin
                .poll_async_events()
                .iter()
                .any(|e| matches!(e, crate::plugin::AsyncEvent::IoChanged)),
            "the host is told"
        );
        assert!(
            !plugin
                .poll_async_events()
                .iter()
                .any(|e| matches!(e, crate::plugin::AsyncEvent::IoChanged)),
            "once"
        );
    }

    /// **A rate change that changes nothing about the layout is silent.**
    ///
    /// Mutation (run): set `self.io_changed = true` unconditionally in
    /// `note_layout_change` → the assertion fails.
    #[test]
    fn a_rate_change_that_keeps_the_layout_does_not_raise_io_changed() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance = ClapInstance::load(path, 44100.0, 512, false).expect("load");
        instance.set_sample_rate(48000.0).expect("accepted");
        assert!(!instance.take_io_changed());
    }

    /// **A 64-bit instance that cannot be re-activated renders silence without
    /// reaching the plugin, reports the failure, and tells the host.** The
    /// plugin drops its 64-bit flags; the rate change fails and leaves it
    /// deactivated, and the session kept calling `process` on it.
    ///
    /// Mutations (run): delete the `!is_active()` early return in
    /// `ClapInstance::process` → the block reaches `ClapActive::process`, which
    /// refuses it with an error → `expect("silence")` fails.
    /// Delete the `self.io_changed = true` in `set_sample_rate`'s error arm →
    /// the `take_io_changed` assertion fails (the layout alone is unchanged in
    /// this fixture).
    #[test]
    fn a_deactivated_instance_renders_silence_and_tells_the_host() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let _flagged = crate::test_utils::clap_probe_ports_64bit(both);
        let mut instance = ClapInstance::load(path, 44100.0, 512, true).expect("load");
        assert!(matches!(instance.inner, ClapInner::F64(_)), "fixture");

        let _dropped = crate::test_utils::clap_probe_ports_64bit(0);
        let err = instance
            .set_sample_rate(48000.0)
            .expect_err("the 64-bit flags are gone");
        assert!(err.to_string().contains("deactivated"), "got {err}");
        assert!(instance.take_io_changed(), "the host is told");

        let input = vec![vec![0.25f64; 64]; 2];
        let input_slices: Vec<&[f64]> = input.iter().map(|v| v.as_slice()).collect();
        let mut output = vec![vec![0.5f64; 64]; 2];
        let mut output_slices: Vec<&mut [f64]> =
            output.iter_mut().map(|v| v.as_mut_slice()).collect();
        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples: 64,
            sample_rate: 44100.0,
        };
        let mut out = tutti_plugin::server::ProcessOutput::default();
        instance
            .process(
                tutti_plugin::server::AudioBufferMut::F64(buffer),
                &ProcessContext::new(),
                &mut out,
            )
            .expect("silence");
        assert!(output.iter().flatten().all(|&s| s == 0.0), "silent");
        assert!(!instance.is_processing(), "the plugin was not started");
    }

    /// The `audio-ports.rescan` flags that need the plugin deactivated.
    ///
    /// The values are `ext/audio-ports.h`'s `CLAP_AUDIO_PORTS_RESCAN_*`.
    const RESCAN_NAMES: u32 = 1 << 0;
    const RESCAN_FLAGS: u32 = 1 << 1;
    const RESCAN_LIST_AND_CHANNELS: u32 = (1 << 5) | (1 << 2);

    /// **A rescan the plugin asks for is served: the instance is
    /// re-activated, its ports are read afresh and the host is told.** The
    /// request was recorded by the host and nothing acted on it, so a plugin
    /// that changed its ports left the host with the layout it was loaded
    /// with.
    ///
    /// Mutations (run): delete the `ports.needs_deactivate()` arm of the
    /// early return in `service_requests` → a request is consumed and
    /// nothing happens → the activation count stays 0 → the second assertion
    /// fails. Replace `i.reactivate()` with `Ok(())` → the layout stays at one
    /// bus → the layout assertion fails. Delete `self.note_layout_change()`
    /// → the host copy stays stale and `take_io_changed` is false → fails.
    #[test]
    fn a_ports_rescan_the_plugin_asks_for_is_served() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance = ClapInstance::load(path, 44100.0, 512, false).expect("load");
        assert_eq!(instance.loaded().inputs.len(), 1, "fixture: one input bus");
        crate::test_utils::clap_probe_reset_activation_count();

        let _layout = crate::test_utils::clap_probe_port_layout(
            tutti_clap_test_plugin::PortLayoutMode::AsymmetricAux as u32,
        );
        let _asked = crate::test_utils::clap_probe_request_ports_rescan(RESCAN_LIST_AND_CHANNELS);
        instance.loaded_mut().on_main_thread();
        assert_eq!(
            instance.loaded().inputs.len(),
            1,
            "nothing is acted on until the server serves it"
        );

        instance.service_requests();

        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            1,
            "one re-activation"
        );
        assert!(instance.clap_loaded().is_active());
        assert_eq!(instance.loaded().inputs.len(), 2, "stereo main + mono aux");
        assert_eq!(instance.loaded().total_inputs(), 3);
        assert!(instance.take_io_changed(), "the host is told");

        instance.service_requests();
        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            1,
            "the request was consumed: not served twice"
        );
    }

    /// **The request reaches the host as `IoChanged` through the server's
    /// between-blocks poll.**
    ///
    /// Mutation (run): delete the `clap.service_requests()` call in
    /// `Plugin::poll_async_events` → no event → fails.
    #[test]
    fn a_served_ports_rescan_is_polled_as_io_changed() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance = ClapInstance::load(path, 44100.0, 512, false).expect("load");
        let _layout = crate::test_utils::clap_probe_port_layout(
            tutti_clap_test_plugin::PortLayoutMode::AsymmetricAux as u32,
        );
        let _asked = crate::test_utils::clap_probe_request_ports_rescan(RESCAN_LIST_AND_CHANNELS);
        instance.loaded_mut().on_main_thread();

        let mut plugin = crate::plugin::Plugin::Clap(instance);
        let io_changed = |plugin: &mut crate::plugin::Plugin| {
            plugin
                .poll_async_events()
                .iter()
                .any(|e| matches!(e, crate::plugin::AsyncEvent::IoChanged))
        };
        assert!(io_changed(&mut plugin), "the host is told");
        assert_eq!(plugin.instance().loaded().total_inputs(), 3);
        assert!(!io_changed(&mut plugin), "once");
    }

    /// **A rescan that re-activates but leaves the layout as it was is
    /// silent**, and a `NAMES`-only rescan, which the host's description does
    /// not carry, does not re-activate at all.
    ///
    /// Mutations (run): set `self.io_changed = true` unconditionally in
    /// `service_requests` → the first `take_io_changed` assertion fails.
    /// Make `needs_deactivate` true for `NAMES` (in `AudioPortsRescan`) → the
    /// names-only count assertion fails.
    #[test]
    fn a_rescan_that_changes_nothing_is_silent() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance = ClapInstance::load(path, 44100.0, 512, false).expect("load");
        crate::test_utils::clap_probe_reset_activation_count();

        let _asked = crate::test_utils::clap_probe_request_ports_rescan(RESCAN_LIST_AND_CHANNELS);
        instance.loaded_mut().on_main_thread();
        instance.service_requests();
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
        assert!(!instance.take_io_changed(), "same layout, same instance");

        let _names = crate::test_utils::clap_probe_request_ports_rescan(RESCAN_NAMES);
        instance.loaded_mut().on_main_thread();
        instance.service_requests();
        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            1,
            "names are safe live: no re-activation"
        );
        assert!(!instance.take_io_changed());
        assert!(
            !instance.clap_loaded().poll_audio_ports_rescan().requested,
            "consumed"
        );
    }

    /// **A `request_restart` is served the same way**: the plugin is
    /// deactivated and activated again, between blocks, and keeps running.
    ///
    /// Mutation (run): drop `|| restart` from the early return in
    /// `service_requests` → the count stays 0 → fails.
    #[test]
    fn a_restart_the_plugin_asks_for_is_served() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance = ClapInstance::load(path, 44100.0, 512, false).expect("load");
        crate::test_utils::clap_probe_reset_activation_count();

        let _asked = crate::test_utils::clap_probe_request_restart();
        process_blocks(&mut instance, 1);
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 0);

        instance.service_requests();
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
        assert!(!instance.take_io_changed(), "same layout, same instance");
        process_blocks(&mut instance, 1);
        assert!(instance.is_processing(), "it runs again");

        instance.service_requests();
        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            1,
            "consumed"
        );
    }

    /// **A 64-bit instance whose plugin drops its flags and asks for a rescan
    /// is left deactivated, and the host is told** even though the layout it
    /// holds did not change.
    ///
    /// Mutation (run): delete the `if active != was_active` block in
    /// `service_requests` → `take_io_changed` is false → fails.
    #[test]
    fn a_rescan_that_leaves_the_instance_unable_to_run_tells_the_host() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let both = tutti_clap_test_plugin::PORTS_64BIT_INPUTS
            | tutti_clap_test_plugin::PORTS_64BIT_OUTPUTS;
        let _flagged = crate::test_utils::clap_probe_ports_64bit(both);
        let mut instance = ClapInstance::load(path, 44100.0, 512, true).expect("load");
        assert!(matches!(instance.inner, ClapInner::F64(_)), "fixture");

        let _dropped = crate::test_utils::clap_probe_ports_64bit(0);
        let _asked = crate::test_utils::clap_probe_request_ports_rescan(RESCAN_FLAGS);
        instance.loaded_mut().on_main_thread();
        instance.service_requests();

        assert!(!instance.clap_loaded().is_active(), "left deactivated");
        assert!(instance.take_io_changed(), "the host is told");
    }

    /// Loads the reference plugin and takes it past one block, so it has asked
    /// for a callback (`request_callback` from `process`).
    fn loaded_and_called_back() -> ClapInstance {
        let mut instance =
            ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false).expect("load");
        process_blocks(&mut instance, 1);
        instance
    }

    /// **A `request_callback` is served: the plugin's `on_main_thread` runs on
    /// the server's service pass, and not before.** The flag was recorded and
    /// nothing called back, so work a plugin deferred to the main thread (a
    /// sample load finishing, a timer being registered) never ran.
    ///
    /// Mutation (run): delete `i.run_main_thread_work()` from
    /// `service_requests` → the latched timer is never registered → fails.
    #[test]
    fn a_callback_the_plugin_asks_for_is_served() {
        let _lock = crate::test_utils::plugin_load_lock();
        crate::test_utils::clap_probe_thread_reset();
        let mut instance =
            ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false).expect("load");
        // `on_main_thread` runs this one; it registers a timer.
        let _cmd = crate::test_utils::clap_probe_thread_command(
            tutti_clap_test_plugin::CMD_REGISTER_TIMER,
        );
        process_blocks(&mut instance, 1);
        let site = tutti_clap_test_plugin::Site::OnMainThread as usize;
        let before = crate::test_utils::clap_probe_thread_capture();
        assert_eq!(before.sites[site].visits, 0, "not before it is served");

        instance.service_requests();

        let after = crate::test_utils::clap_probe_thread_capture();
        assert_eq!(after.sites[site].visits, 1);
        assert!(after.timer_registered, "the latched command ran in it");
    }

    /// **A timer the plugin registers ticks on the service pass.** The host
    /// answered `register_timer` with `true` and never called `on_timer`.
    ///
    /// Mutation (run): delete the `poll_timers_at(now)` call from
    /// `ClapLoaded::run_main_thread_work` → no tick → fails.
    #[test]
    fn a_timer_the_plugin_registers_is_ticked() {
        let _lock = crate::test_utils::plugin_load_lock();
        crate::test_utils::clap_probe_thread_reset();
        let mut instance = loaded_and_called_back();
        let _cmd = crate::test_utils::clap_probe_thread_command(
            tutti_clap_test_plugin::CMD_REGISTER_TIMER,
        );
        // Run the callback that registers the timer (the block's request has
        // gone unserved), then let the timer tick.
        instance.loaded_mut().on_main_thread();
        let site = tutti_clap_test_plugin::Site::OnTimer as usize;
        assert!(crate::test_utils::clap_probe_thread_capture().timer_registered);
        assert_eq!(
            crate::test_utils::clap_probe_thread_capture().sites[site].visits,
            0
        );

        instance.service_requests();

        let cap = crate::test_utils::clap_probe_thread_capture();
        assert_eq!(cap.sites[site].visits, 1, "a period-0 timer is due at once");
        assert_eq!(cap.last_fired_timer_id, cap.timer_id);
    }

    /// **A latency the plugin settles on at activation is reported after a
    /// sample-rate change**, with no `changed()` from the plugin: the figure
    /// the host holds is from the old rate.
    ///
    /// Mutation (run): delete `self.note_latency_change(false)` from
    /// `ClapInstance::set_sample_rate` → no change is reported → fails.
    #[test]
    fn a_latency_that_depends_on_the_rate_is_reread_after_a_rate_change() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false).expect("load");
        let before = instance.loaded().latency_samples;
        assert_eq!(instance.take_changes().latency, None);

        let _longer = crate::test_utils::clap_probe_latency_at_activation(11);
        instance.set_sample_rate(48000.0).expect("rate change");

        let now = Samples(before.0 + 11);
        assert_eq!(instance.take_changes().latency, Some(now));
        assert_eq!(instance.loaded().latency_samples, now, "the host's copy");
        assert_eq!(instance.take_changes().latency, None, "once");
    }

    /// **A `request_restart` reports the latency the plugin has after it.**
    ///
    /// Mutation (run): delete `self.note_latency_change(latency)` under
    /// `if reactivate` in `service_requests` → fails.
    #[test]
    fn a_restart_reports_the_latency_after_it() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false).expect("load");
        let before = instance.loaded().latency_samples;
        let _longer = crate::test_utils::clap_probe_latency_at_activation(7);
        let _asked = crate::test_utils::clap_probe_request_restart();
        process_blocks(&mut instance, 1);

        instance.service_requests();

        assert_eq!(instance.take_changes().latency, Some(Samples(before.0 + 7)));
    }

    /// **`latency.changed` on an active plugin is served by a re-activation,
    /// and the latency is read after it**, not from the active plugin
    /// (`clap.latency`: it may change only across an activation).
    ///
    /// Mutation (run): drop `|| (latency && was_active)` from the condition in
    /// `service_requests` → no re-activation, so the figure is never re-read →
    /// both assertions fail.
    #[test]
    fn latency_changed_on_an_active_plugin_restarts_it() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance =
            ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false).expect("load");
        let before = instance.loaded().latency_samples;
        crate::test_utils::clap_probe_reset_activation_count();
        let _longer = crate::test_utils::clap_probe_latency_at_activation(5);
        instance
            .clap_loaded()
            .host_state()
            .processing
            .latency_changed
            .store(true, std::sync::atomic::Ordering::Release);

        instance.service_requests();

        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
        assert_eq!(instance.take_changes().latency, Some(Samples(before.0 + 5)));
    }

    /// **A `latency.changed()` a plugin raises from inside `activate` is
    /// answered by that activation**: one request is one activation, not a
    /// second restart, and not a loop when the plugin does it every time.
    ///
    /// Mutation (run): delete the two `store(false, ..)` calls at the end of
    /// `ClapLoaded::activate_plugin` → the load-time announcement is still
    /// latched, so the first `service_requests` re-activates for it (the first
    /// count assertion reads 1), and each activation re-latches it (no loop
    /// guard).
    #[test]
    fn a_latency_changed_inside_activate_is_not_restarted_for() {
        let _lock = crate::test_utils::plugin_load_lock();
        let _announces = crate::test_utils::clap_probe_latency_changed_in_activate();
        let mut instance =
            ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false).expect("load");
        crate::test_utils::clap_probe_reset_activation_count();

        instance.service_requests();
        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            0,
            "the announcement made while loading was answered by the load"
        );

        // One request, one activation, whose own announcement is answered too.
        instance
            .clap_loaded()
            .host_state()
            .processing
            .latency_changed
            .store(true, std::sync::atomic::Ordering::Release);
        instance.service_requests();
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
        instance.service_requests();
        instance.service_requests();
        assert_eq!(
            crate::test_utils::clap_probe_activation_count(),
            1,
            "no loop"
        );
        assert!(!instance.clap_loaded().poll_latency_changed());
    }

    /// **A `params.rescan` of values is reported as changed values and does not
    /// restart the plugin.** The flags were latched and nothing read them.
    ///
    /// Mutation (run): delete the `params.values || params.text` arm in
    /// `service_requests` → nothing is reported → fails.
    #[test]
    fn a_values_rescan_is_reported_without_a_restart() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = loaded_and_called_back();
        crate::test_utils::clap_probe_reset_activation_count();
        let _asked = crate::test_utils::clap_probe_param_command(
            tutti_clap_test_plugin::PARAM_CMD_RESCAN_VALUES,
        );

        instance.service_requests();

        let changes = instance.take_changes();
        assert!(changes.param_values);
        assert!(!changes.param_titles);
        assert_eq!(crate::test_utils::clap_probe_activation_count(), 0);
    }

    /// **A `params.rescan(ALL)` restarts the plugin and is reported as changed
    /// parameters**, so the host re-pulls the list.
    ///
    /// Mutations (run): drop `|| params.needs_deactivate()` from the condition
    /// in `service_requests` → no re-activation → the count assertion fails.
    /// Delete `self.pending.param_titles = true` under `params.info ||
    /// params.all` → the report assertion fails. What the re-read changes (the
    /// ranges automation is denormalized against) is not observable with a
    /// plugin whose parameters are fixed.
    #[test]
    fn an_all_rescan_restarts_the_plugin_and_reports_the_list() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = loaded_and_called_back();
        crate::test_utils::clap_probe_reset_activation_count();
        let _asked = crate::test_utils::clap_probe_param_command(
            tutti_clap_test_plugin::PARAM_CMD_RESCAN_ALL,
        );

        instance.service_requests();

        assert_eq!(crate::test_utils::clap_probe_activation_count(), 1);
        assert!(instance.clap_loaded().is_active());
        assert!(instance.take_changes().param_titles);
    }

    #[test]
    fn test_clap_parameter_list() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let instance =
            ClapInstance::load(path, 44100.0, 512, false).expect("Failed to load CLAP plugin");

        let params = instance.get_parameter_list();
        assert!(
            !params.is_empty(),
            "Parameter list should not be empty for TAL-NoiseMaker"
        );

        for param in &params {
            assert!(
                !param.name.is_empty(),
                "Parameter id {} has empty name",
                param.id
            );
        }
    }

    /// CLAP lists nothing and can still load — a coherent state, not a
    /// contradiction.
    ///
    /// The mirror of VST3, which lists richly and cannot load. CLAP's
    /// enumeration lives in the factory-level preset-discovery extension, which
    /// is queried on the *factory* rather than a loaded instance and which this
    /// host does not bind; `CLAP_EXT_PRESET_LOAD` answers only "can this be
    /// pointed at a path".
    ///
    /// So the empty list is **not** the same claim AU's empty list makes.
    /// `PRESET_LIST` is unprobed for CLAP, so a caller reads `None` — nobody
    /// asked — where AU reports `Some(false)`, meaning the unit declined. That
    /// distinction is the whole reason the two bits are separate, and asserting
    /// it here is what keeps a future "simplification" from collapsing them.
    #[test]
    fn clap_lists_nothing_and_still_reports_a_load_capability() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance =
            ClapInstance::load(path, 44100.0, 512, false).expect("Failed to load CLAP plugin");

        assert!(
            instance.get_presets().is_empty(),
            "CLAP has no instance-level enumeration; a non-empty list would mean \
             something invented one"
        );
        assert_eq!(
            instance.get_current_preset(),
            None,
            "CLAP has no current-preset query either"
        );

        let loaded = instance.loaded();
        let report = tutti_plugin::server::FeatureReport::new(loaded.probed, loaded.features);
        assert_eq!(
            report.get(tutti_plugin::server::Features::PRESET_LIST),
            None,
            "PRESET_LIST must read as unasked for CLAP, not as a declination"
        );
    }

    /// Only a path-shaped id reaches CLAP's loader.
    ///
    /// The preset analogue of `clap_refuses_a_vst2_index`: `from_location`
    /// takes a filesystem path and nothing else, so a `Number` or `Program`
    /// names no CLAP preset. There is not even a number to coerce it *to*,
    /// which is why the id is opaque rather than an integer.
    #[test]
    fn clap_refuses_an_id_that_is_not_a_path() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance =
            ClapInstance::load(path, 44100.0, 512, false).expect("Failed to load CLAP plugin");

        // Asserted on the guard, not through `load_preset`: a coerced id
        // yields a nonexistent path the plugin refuses anyway, so the two
        // refusals look identical from outside.
        assert_eq!(
            clap_preset_path(&PresetId::Number(0)),
            None,
            "an AU selector / VST2 index addresses no CLAP preset"
        );
        assert_eq!(
            clap_preset_path(&PresetId::Program {
                list_id: 0,
                index: 0
            }),
            None,
            "a VST3 program id addresses no CLAP preset"
        );
        assert_eq!(
            clap_preset_path(&PresetId::Location("/x.clap-preset".into())),
            Some(Path::new("/x.clap-preset")),
            "a path-shaped id reaches the loader verbatim"
        );

        // And the whole path still refuses, which is what a caller sees.
        assert!(!instance.load_preset(&PresetId::Number(0)));
    }

    /// A VST2 index reaching a CLAP plugin addresses nothing, and must be
    /// refused rather than read as an id.
    ///
    /// `ParamAddress` makes the two models distinguishable; this pins that the
    /// loader acts on the distinction instead of unwrapping the number. Without
    /// it the enum is only documentation — the shared `u32` this replaced would
    /// have handed index 0 straight to `clap_id` 0.
    #[test]
    fn clap_refuses_a_vst2_index() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance =
            ClapInstance::load(path, 44100.0, 512, false).expect("Failed to load CLAP plugin");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "Need at least one parameter");
        let opaque = params[0].id;
        let before = instance.get_parameter(opaque);

        // Same number, wrong model.
        let raw = opaque.opaque().expect("CLAP ids are opaque").get();
        let as_index = ParamAddress::Index(raw as i32);

        assert_eq!(
            instance.get_parameter(as_index),
            0.0,
            "an index addresses no CLAP parameter, so the read reports the              unknown-parameter value rather than reading id {raw}"
        );

        // And the write must not land either.
        let target = if before > 0.5 { 0.1 } else { 0.9 };
        instance.set_parameter(as_index, Normalized::new(target));
        assert_eq!(
            instance.get_parameter(opaque),
            before,
            "a write addressed by index must be dropped, not applied to the              parameter that happens to own that number"
        );
    }

    #[test]
    fn test_clap_get_parameter() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let instance =
            ClapInstance::load(path, 44100.0, 512, false).expect("Failed to load CLAP plugin");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "Need at least one parameter");

        let first_id = params[0].id;
        let value = instance.get_parameter(first_id);
        assert!(
            value.is_finite(),
            "Parameter value should be finite, got {}",
            value
        );
    }

    /// Arm (or, with 0, disarm) the reference plugin's thread-pool probe on
    /// the image the loader opens; see `set_clap_probe_render_mode` for why
    /// through `dlopen`.
    fn set_thread_pool_tasks(tasks: u32) {
        // SAFETY: `#[no_mangle] extern "C" fn(u32)` in the reference plugin.
        unsafe {
            let lib = libloading::Library::new(clap_plugin()).expect("reference plugin opens");
            let set: libloading::Symbol<unsafe extern "C" fn(u32)> = lib
                .get(b"tutti_test_plugin_set_thread_pool_tasks\0")
                .expect("thread-pool switch exported");
            set(tasks);
            // Leaked: closing it before the load would unmap the switch.
            std::mem::forget(lib);
        }
    }

    fn thread_pool_capture() -> tutti_clap_test_plugin::ThreadPoolCapture {
        // SAFETY: `#[no_mangle] extern "C" fn() -> ThreadPoolCapture`, the
        // same `#[repr(C)]` type as the rlib's.
        unsafe {
            let lib = libloading::Library::new(clap_plugin()).expect("reference plugin opens");
            let get: libloading::Symbol<
                unsafe extern "C" fn() -> tutti_clap_test_plugin::ThreadPoolCapture,
            > = lib
                .get(b"tutti_test_plugin_thread_pool_capture\0")
                .expect("thread-pool capture exported");
            let cap = get();
            std::mem::forget(lib);
            cap
        }
    }

    fn process_blocks(instance: &mut ClapInstance, blocks: usize) {
        let num_samples = 64;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        for _ in 0..blocks {
            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();
            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };
            let mut out = tutti_plugin::server::ProcessOutput::default();
            instance
                .process(
                    tutti_plugin::server::AudioBufferMut::F32(buffer),
                    &ProcessContext::new(),
                    &mut out,
                )
                .expect("block processes");
        }
    }

    /// A plugin that implements `clap.thread-pool` has its requests served
    /// by the server's own pool: the host's pool is in another process.
    ///
    /// Mutation: drop the `set_thread_pool` call in `ClapInstance::load` →
    /// every request is refused → `accepted` is 0 → fails (on 4 cores or
    /// more; below that the server builds no pool and the test checks the
    /// refusal).
    #[test]
    fn a_thread_pool_plugin_has_its_requests_served_here() {
        let _lock = crate::test_utils::plugin_load_lock();
        set_thread_pool_tasks(8);
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load CLAP plugin");
        process_blocks(&mut instance, 4);
        set_thread_pool_tasks(0);
        let cap = thread_pool_capture();
        let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
        // Below 4 cores the server builds no pool, and requests are refused.
        let served = if pool_participants(cores) >= 2 { 4 } else { 0 };
        assert_eq!(cap.requests, 4, "{cap:?}");
        assert_eq!((cap.accepted, cap.refused), (served, 4 - served), "{cap:?}");
        assert_eq!(cap.accepted_incomplete, 0);
    }

    /// A plugin without the extension costs no threads: the pool is built
    /// only for one that has it.
    ///
    /// `THREAD_POOL` is process-global, so this relies on nextest running
    /// each test in its own process: under `cargo test`, a thread-pool test
    /// that ran first in the same process would have built it.
    ///
    /// Mutation: build the pool unconditionally in `ClapInstance::load` →
    /// it exists after this load → fails.
    /// Half the cores, at most 4, and below 2 no pool.
    ///
    /// Mutation: drop the `.min(4)` → 64 cores give 32 → fails.
    #[test]
    fn the_server_pool_takes_half_the_cores_up_to_four() {
        let sizes: Vec<usize> = [1, 2, 3, 4, 6, 8, 64].map(pool_participants).to_vec();
        assert_eq!(sizes, [0, 1, 1, 2, 3, 4, 4]);
    }

    #[test]
    fn a_plugin_without_a_thread_pool_builds_no_pool() {
        let _lock = crate::test_utils::plugin_load_lock();
        set_thread_pool_tasks(0);
        let _instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load CLAP plugin");
        assert!(THREAD_POOL.get().is_none());
    }

    #[test]
    fn test_clap_process_f32_silence() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance =
            ClapInstance::load(path, 44100.0, 512, false).expect("Failed to load CLAP plugin");

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new();
        // Should not panic
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    /// The host feeds a NoteOn and collects audio back.
    ///
    /// The reference plugin is an effect, not a synth, so what this pins is the
    /// *round trip* — events reach the plugin and its output reaches the host —
    /// rather than note-to-pitch synthesis. `TagOnly` makes the output side
    /// observable: the probe writes a nonzero tag into every slot regardless of
    /// input, so a host that dropped the output buffers reads as silence.
    #[test]
    fn test_clap_process_f32_with_note() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(clap_plugin());
        let mut instance =
            ClapInstance::load(path, 44100.0, 512, false).expect("Failed to load CLAP plugin");
        crate::test_utils::set_clap_probe_render_mode(
            crate::test_utils::CLAP_PROBE_RENDER_TAG_ONLY,
        );

        let num_samples = 512;

        // First block: send a NoteOn event
        let note_on = [tutti_plugin::server::MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        )];

        let mut has_nonzero = false;

        // Process the block with NoteOn
        {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let ctx = ProcessContext::new().midi(&note_on);
            let mut out = tutti_plugin::server::ProcessOutput::default();
            let _output = instance.process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &ctx,
                &mut out,
            );

            // Check output for non-zero samples
            for ch in output_data.iter() {
                for &sample in ch.iter() {
                    if sample != 0.0 {
                        has_nonzero = true;
                    }
                }
            }
        }

        // Process additional blocks (at least 4 more) to give the synth time to produce sound
        let empty_ctx = ProcessContext::new();
        for _ in 0..4 {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let mut out = tutti_plugin::server::ProcessOutput::default();
            let _output = instance.process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &empty_ctx,
                &mut out,
            );

            for ch in output_data.iter() {
                for &sample in ch.iter() {
                    if sample != 0.0 {
                        has_nonzero = true;
                    }
                }
            }
        }

        assert!(
            has_nonzero,
            "Expected at least one non-zero output sample after NoteOn"
        );
    }

    // ── Second-instance tests ──
    //
    // These ran against a second, separately-installed plugin (Surge XT at an
    // absolute macOS path). The group was headed "f64-capable", but nothing in
    // it asserts f64 — Surge does not advertise
    // `CLAP_AUDIO_PORT_SUPPORTS_64BITS` either, as `test_surge_port_flags`
    // itself notes. What the group actually covers is a *second* load of the
    // host against a different instance: stereo output, process-without-crash,
    // state round-trip, voice info. Pointing it at the reference plugin keeps
    // that coverage and lets it run anywhere.
    //
    // The f64 suite is the session's (`an_f64_session_runs_a_64bit_capable_clap_plugin_in_f64`
    // and its neighbours): the reference plugin advertises 64-bit ports behind
    // `tutti_test_plugin_set_ports_64bit`.

    #[test]
    fn test_surge_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false);
        assert!(
            instance.is_ok(),
            "Failed to load Surge XT: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        let meta = instance.descriptor();
        assert!(!meta.name.is_empty());
        assert!(!meta.id.is_empty());
        println!("Surge XT loaded: {} ({})", meta.name, meta.id);
    }

    #[test]
    fn test_surge_port_flags() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load Surge XT");

        let port_info = instance.clap_loaded().audio_port_info(0, false);
        assert!(port_info.is_some(), "Expected at least one output port");

        let port = port_info.unwrap();
        assert!(port.layout.count() >= 2, "Expected stereo output");
    }

    #[test]
    fn test_surge_process_f32_silence() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load Surge XT");

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new();
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    #[test]
    fn test_surge_process_f32_with_note() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load Surge XT");

        let num_samples = 512;
        let note_on = [tutti_plugin::server::MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        )];

        // Process block with NoteOn + follow-up blocks.
        // Surge XT is strict about CLAP thread contracts and may not produce
        // audio when process() is called from a non-audio thread, so this only
        // verify no crash rather than asserting on output content.
        for i in 0..5 {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let ctx = if i == 0 {
                ProcessContext::new().midi(&note_on)
            } else {
                ProcessContext::new()
            };

            let mut out = tutti_plugin::server::ProcessOutput::default();
            let _output = instance.process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &ctx,
                &mut out,
            );
        }
    }

    /// Test Surge XT parameter setting via flush.
    #[test]
    fn test_surge_set_parameter_flush() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load Surge XT");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "Need at least one parameter");

        let param_id = params[0].id;
        instance.set_parameter(param_id, Normalized::new(0.5));
    }

    // ── Group A: Plugin Lifecycle ──

    #[test]
    fn test_clap_lifecycle_active_after_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");
        // `load()` activates the plugin: `inner` is a `ClapActive` arm, so the
        // "active after load" guarantee is now encoded in the type itself.
        assert!(
            matches!(instance.inner, ClapInner::F32(_) | ClapInner::F64(_)),
            "load() should yield an active (ClapActive) instance"
        );
    }

    #[test]
    fn test_clap_lifecycle_processing_starts_on_process() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        // A freshly-activated instance has not started processing yet — that
        // happens lazily on the first `process` call (the type guarantees it is
        // already activated, so there is no "auto-activate" step to test).
        assert!(
            !instance.is_processing(),
            "Should not be processing before first process()"
        );

        let num_samples = 128;
        let input_data: Vec<Vec<f32>> = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();
        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };
        let mut out = tutti_plugin::server::ProcessOutput::default();
        instance
            .process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &ProcessContext::new(),
                &mut out,
            )
            .expect("process should succeed");

        assert!(
            instance.is_processing(),
            "Should be processing after first process()"
        );
    }

    // ── Group B: Polling Methods ──

    #[test]
    fn test_clap_poll_initially_clear() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        // All poll methods should return false on a fresh instance
        assert!(!instance.clap_loaded().poll_restart_requested());
        assert!(!instance.clap_loaded().poll_process_requested());
        assert!(!instance.clap_loaded().poll_callback_requested());
        assert!(!instance.clap_loaded().poll_latency_changed());
        assert!(!instance.clap_loaded().poll_tail_changed());
        assert!(!instance.clap_loaded().poll_params_rescan().requested);
        assert!(!instance.clap_loaded().poll_params_flush_requested());
        assert!(!instance.clap_loaded().poll_state_dirty());
        assert!(!instance.clap_loaded().poll_audio_ports_rescan().requested);
        assert!(!instance.clap_loaded().poll_note_ports_changed());
    }

    #[test]
    fn test_clap_poll_restart() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let state = instance.clap_loaded().host_state();
        state
            .lifecycle
            .restart_requested
            .store(true, Ordering::Release);

        // needs_restart() is non-clearing — should return true repeatedly
        assert!(
            instance.clap_loaded().needs_restart(),
            "needs_restart should be true"
        );
        assert!(
            instance.clap_loaded().needs_restart(),
            "needs_restart should still be true (non-clearing)"
        );

        // poll_restart_requested() clears the flag
        assert!(
            instance.clap_loaded().poll_restart_requested(),
            "poll should return true"
        );
        assert!(
            !instance.clap_loaded().poll_restart_requested(),
            "poll should return false after clearing"
        );
        assert!(
            !instance.clap_loaded().needs_restart(),
            "needs_restart should be false after poll cleared it"
        );
    }

    #[test]
    fn test_clap_poll_process_callback() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let state = instance.clap_loaded().host_state();
        state
            .lifecycle
            .process_requested
            .store(true, Ordering::Release);
        state
            .lifecycle
            .callback_requested
            .store(true, Ordering::Release);

        assert!(instance.clap_loaded().poll_process_requested());
        assert!(instance.clap_loaded().poll_callback_requested());

        // Second poll should be false (cleared)
        assert!(!instance.clap_loaded().poll_process_requested());
        assert!(!instance.clap_loaded().poll_callback_requested());
    }

    #[test]
    fn test_clap_poll_latency_tail() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let state = instance.clap_loaded().host_state();
        state
            .processing
            .latency_changed
            .store(true, Ordering::Release);
        state.processing.tail_changed.store(true, Ordering::Release);

        assert!(instance.clap_loaded().poll_latency_changed());
        assert!(instance.clap_loaded().poll_tail_changed());

        assert!(!instance.clap_loaded().poll_latency_changed());
        assert!(!instance.clap_loaded().poll_tail_changed());
    }

    #[test]
    fn test_clap_poll_ports_state() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let state = instance.clap_loaded().host_state();
        state.audio_ports.changed.store(true, Ordering::Release);
        state.notes.ports_changed.store(true, Ordering::Release);
        state.processing.state_dirty.store(true, Ordering::Release);

        assert!(instance.clap_loaded().poll_audio_ports_rescan().requested);
        assert!(instance.clap_loaded().poll_note_ports_changed());
        assert!(instance.clap_loaded().poll_state_dirty());

        assert!(!instance.clap_loaded().poll_audio_ports_rescan().requested);
        assert!(!instance.clap_loaded().poll_note_ports_changed());
        assert!(!instance.clap_loaded().poll_state_dirty());
    }

    // ── Group C: Parameter Get/Set Roundtrip ──

    #[test]
    fn test_clap_param_set_get_roundtrip() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "Need at least one parameter");

        let param_id = params[0].id;
        let original = instance.get_parameter(param_id);

        // Set to a different value
        let new_value = if original < 0.5 { 0.75 } else { 0.25 };
        instance.set_parameter(param_id, Normalized::new(new_value));

        let readback = instance.get_parameter(param_id);
        assert!(
            (readback - new_value).abs() < 0.01,
            "Parameter should be close to set value: expected {}, got {}",
            new_value,
            readback
        );
    }

    // ── Group D: State Save/Load Roundtrip ──

    #[test]
    fn test_clap_state_save_nonempty() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let state = instance.get_state().expect("save_state should succeed");
        assert!(!state.is_empty(), "Saved state should not be empty");
    }

    #[test]
    fn test_clap_state_save_load_roundtrip() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "Need at least one parameter");
        let param_id = params[0].id;

        // Save original state
        let saved = instance.get_state().expect("save should succeed");
        let original_value = instance.get_parameter(param_id);

        // Change parameter
        let new_value = if original_value < 0.5 { 0.75 } else { 0.25 };
        instance.set_parameter(param_id, Normalized::new(new_value));

        // Restore state
        instance.set_state(&saved).expect("restore should succeed");

        let restored_value = instance.get_parameter(param_id);
        assert!(
            (restored_value - original_value).abs() < 0.01,
            "Parameter should be restored: expected {}, got {}",
            original_value,
            restored_value
        );
    }

    // ── Group E: Audio/Note Port Enumeration ──

    #[test]
    fn test_clap_audio_port_enumeration() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let output_count = instance.clap_loaded().audio_port_count(false);
        assert!(
            output_count > 0,
            "Synth should have at least one output port"
        );

        for i in 0..output_count {
            let info = instance
                .clap_loaded()
                .audio_port_info(i, false)
                .expect("audio_port_info should return Some");
            assert!(info.layout.count() > 0, "Port {} should have channels", i);
            assert!(!info.name.is_empty(), "Port {} should have a name", i);
        }
    }

    #[test]
    fn test_clap_note_port_enumeration() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let input_count = instance.clap_loaded().note_port_count(true);
        assert!(
            input_count > 0,
            "Synth should have at least one note input port"
        );

        for i in 0..input_count {
            let info = instance
                .clap_loaded()
                .note_port_info(i, true)
                .expect("note_port_info should return Some");
            assert!(!info.name.is_empty(), "Note port {} should have a name", i);
        }
    }

    // ── Group F: Latency/Tail Queries ──

    #[test]
    fn test_clap_latency_and_tail() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        // Just verify no crash — values depend on plugin
        let _latency = instance.clap_loaded().get_latency();
        let _tail = instance.clap_loaded().get_tail();
    }

    // ── Group G: Processing with Param Automation, Expressions, Transport ──

    #[test]
    fn test_clap_process_with_param_automation() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty());

        let mut changes = ParameterChanges::new();
        let mut queue = ParameterQueue::with_capacity(params[0].id, 1);
        queue.add_point(0, 0.5);
        changes.add_queue(queue);

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new().params(&changes);
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    #[test]
    fn test_clap_process_with_note_expression() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let note_on = [tutti_plugin::server::MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        )];

        let mut expr_changes = NoteExpressionChanges::new();
        expr_changes.add_change(NoteExpressionValue {
            sample_offset: 0,
            note_id: -1,
            expression_type: NoteExpressionType::Volume,
            value: 0.8,
        });

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new()
            .midi(&note_on)
            .note_expression(&expr_changes);
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    #[test]
    fn test_clap_process_with_transport() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let transport = TransportInfo::new()
            .with_playing(true)
            .with_tempo(120.0)
            .with_time_signature(tutti_plugin::server::TimeSignature::default());

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = ProcessContext::new().transport(&transport);
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    // ── Group I: Surge XT Additional Coverage ──

    #[test]
    fn test_surge_state_save_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load Surge XT");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty());
        let param_id = params[0].id;

        let saved = instance.get_state().expect("save should succeed");
        assert!(!saved.is_empty(), "Surge XT state should not be empty");

        let original_value = instance.get_parameter(param_id);
        let new_value = if original_value < 0.5 { 0.75 } else { 0.25 };
        instance.set_parameter(param_id, Normalized::new(new_value));

        instance.set_state(&saved).expect("restore should succeed");

        let restored = instance.get_parameter(param_id);
        assert!(
            (restored - original_value).abs() < 0.01,
            "Surge XT param should be restored: expected {}, got {}",
            original_value,
            restored
        );
    }

    #[test]
    fn test_surge_audio_ports() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load Surge XT");

        let output_count = instance.clap_loaded().audio_port_count(false);
        assert!(output_count > 0, "Surge XT should have output ports");

        let port = instance
            .clap_loaded()
            .audio_port_info(0, false)
            .expect("Should have at least one output port");
        assert!(
            port.layout.count() >= 2,
            "Expected stereo output, got {} channels",
            port.layout.count()
        );
    }

    #[test]
    fn test_surge_latency_tail() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load Surge XT");

        let _latency = instance.clap_loaded().get_latency();
        let _tail = instance.clap_loaded().get_tail();
    }

    // ── Group J: GUI / Editor ──

    /// Editor presence is *answered*, not left unknown.
    ///
    /// The reference plugin exposes no `clap.gui`, so the expected answer here
    /// is `Absent` — the load path must have asked and recorded the negative.
    /// Asserting `!= Unknown` is the load-bearing half: `Unknown` is the
    /// `Default`, so a descriptor built by struct-update that never consulted
    /// the plugin would carry it, and a host that skipped the query entirely
    /// would look identical to one that asked and got "no". Pinning `Absent`
    /// specifically also keeps a host that guesses `Present` for every plugin
    /// from passing.
    #[test]
    fn test_clap_has_gui() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");
        assert_eq!(
            instance.descriptor().editor,
            EditorPresence::Absent,
            "the reference plugin exposes no clap.gui, so the host must report \
             a queried absence rather than the `Unknown` default"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore] // Requires main-thread Cocoa environment; run with: cargo test -- --ignored test_clap_gui
    fn test_clap_gui_open_close() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let parent = create_nsview();
        assert!(!parent.is_null(), "Failed to create NSView");

        let handle = unsafe { tutti_plugin::server::WindowHandle::from_u64(parent as u64) };
        let result = instance.open_editor(handle, ContentScale::IDENTITY);
        assert!(
            result.is_ok(),
            "open_editor should succeed: {:?}",
            result.err()
        );

        let size = result.unwrap();
        assert!(
            size.width > 0 && size.height > 0,
            "GUI size should be non-zero: {}x{}",
            size.width,
            size.height
        );

        instance.close_editor();
        release_nsview(parent);
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore] // Requires main-thread Cocoa environment; run with: cargo test -- --ignored test_surge_gui
    fn test_surge_gui_open_close() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load Surge XT");

        assert!(
            instance.descriptor().editor.is_present(),
            "Surge XT should have a GUI"
        );

        let parent = create_nsview();
        assert!(!parent.is_null());

        let handle = unsafe { tutti_plugin::server::WindowHandle::from_u64(parent as u64) };
        let result = instance.open_editor(handle, ContentScale::IDENTITY);
        assert!(
            result.is_ok(),
            "Surge XT open_editor should succeed: {:?}",
            result.err()
        );

        let size = result.unwrap();
        assert!(
            size.width > 0 && size.height > 0,
            "Surge XT GUI size should be non-zero: {}x{}",
            size.width,
            size.height
        );

        instance.close_editor();
        release_nsview(parent);
    }

    // ── Group K: Render Mode ──

    #[test]
    fn test_clap_render_mode() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        // Set offline mode — returns true if plugin supports render extension
        let _ = instance.loaded_mut().set_render_mode(true);
        // Set back to realtime
        let _ = instance.loaded_mut().set_render_mode(false);
        // No crash is the assertion
    }

    #[test]
    fn test_clap_has_hard_realtime_requirement() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        // Most synths don't have hard RT requirements
        let _has_rt = instance.clap_loaded().has_hard_realtime_requirement();
    }

    // ── Group L: Voice Info ──

    #[test]
    fn test_clap_voice_info() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        // TAL-NoiseMaker may or may not support voice info
        if let Some(info) = instance.clap_loaded().get_voice_info() {
            assert!(info.voice_count > 0, "voice_count should be > 0");
            assert!(info.voice_capacity > 0, "voice_capacity should be > 0");
        }
    }

    #[test]
    fn test_surge_voice_info() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load Surge XT");

        // Surge XT likely supports voice info
        if let Some(info) = instance.clap_loaded().get_voice_info() {
            assert!(info.voice_count > 0, "Surge XT voice_count should be > 0");
            assert!(
                info.voice_capacity > 0,
                "Surge XT voice_capacity should be > 0"
            );
        }
    }

    // ── Group M: Note Names ──

    #[test]
    fn test_clap_note_names() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        let count = instance.clap_loaded().note_name_count();
        // Iterate whatever's there — may be 0 for synths without custom note names
        for i in 0..count {
            let name = instance.clap_loaded().get_note_name(i);
            assert!(name.is_some(), "note_name at index {} should exist", i);
            assert!(
                !name.unwrap().name.is_empty(),
                "note_name {} should have a name",
                i
            );
        }
    }

    // ── Group N: Sample Rate Cycling ──

    #[test]
    fn test_clap_sample_rate_change() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        // Change sample rate — this deactivates, updates, and requires reactivation
        instance.set_sample_rate(48000.0).expect("rate accepted");

        // Process should still work (auto-reactivates)
        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];
        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = tutti_plugin::server::AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 48000.0,
        };

        let ctx = ProcessContext::new();
        let mut out = tutti_plugin::server::ProcessOutput::default();
        let _output = instance.process(
            tutti_plugin::server::AudioBufferMut::F32(buffer),
            &ctx,
            &mut out,
        );
    }

    #[test]
    fn test_clap_sample_rate_cycle_multiple() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        // Cycle through several sample rates
        for &rate in &[48000.0, 96000.0, 44100.0, 22050.0] {
            instance.set_sample_rate(rate).expect("rate accepted");

            let num_samples = 512;
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];
            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = tutti_plugin::server::AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: rate,
            };

            let ctx = ProcessContext::new();
            let mut out = tutti_plugin::server::ProcessOutput::default();
            let _output = instance.process(
                tutti_plugin::server::AudioBufferMut::F32(buffer),
                &ctx,
                &mut out,
            );
        }
    }

    // ── Group O: State Context ──

    #[test]
    fn test_clap_state_context_support() {
        let _lock = crate::test_utils::plugin_load_lock();
        let instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        // Just query — may or may not be supported
        let _supports = instance.clap_loaded().supports_state_context();
    }

    #[test]
    fn test_clap_state_context_save_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        // Save with ForProject context (falls back to regular save if unsupported)
        let saved = instance
            .clap_loaded()
            .state_with_context(tutti_clap_host::StateContext::ForProject)
            .expect("state_with_context should succeed");
        assert!(!saved.is_empty());

        // Load it back
        instance
            .loaded_mut()
            .set_state_with_context(&saved, tutti_clap_host::StateContext::ForProject)
            .expect("set_state_with_context should succeed");
    }

    #[test]
    fn test_clap_state_context_for_duplicate() {
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = ClapInstance::load(Path::new(clap_plugin()), 44100.0, 512, false)
            .expect("Failed to load");

        // ForDuplicate context — used when duplicating a plugin instance
        let saved = instance
            .clap_loaded()
            .state_with_context(tutti_clap_host::StateContext::ForDuplicate)
            .expect("save should succeed");
        assert!(!saved.is_empty());

        instance
            .loaded_mut()
            .set_state_with_context(&saved, tutti_clap_host::StateContext::ForDuplicate)
            .expect("load should succeed");
    }

    // Objective-C runtime FFI for NSView creation (macOS only).
    // On Apple Silicon, objc_msgSend must be cast to the correct function
    // pointer type — the variadic extern "C" declaration doesn't work.
    #[cfg(target_os = "macos")]
    extern "C" {
        fn objc_getClass(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn sel_registerName(name: *const std::ffi::c_char) -> *mut std::ffi::c_void;
        fn objc_msgSend();
    }

    #[cfg(target_os = "macos")]
    type ObjcMsgSendFn =
        unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void) -> *mut std::ffi::c_void;

    #[cfg(target_os = "macos")]
    fn objc_send() -> ObjcMsgSendFn {
        unsafe { std::mem::transmute(objc_msgSend as *const ()) }
    }

    /// Ensure NSApplication is initialized (required for plugin GUIs).
    #[cfg(target_os = "macos")]
    fn ensure_nsapp() {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| unsafe {
            let send = objc_send();
            let cls = objc_getClass(c"NSApplication".as_ptr());
            let shared_sel = sel_registerName(c"sharedApplication".as_ptr());
            send(cls, shared_sel);
        });
    }

    #[cfg(target_os = "macos")]
    fn create_nsview() -> *mut std::ffi::c_void {
        ensure_nsapp();
        unsafe {
            let send = objc_send();
            let cls = objc_getClass(c"NSView".as_ptr());
            let alloc_sel = sel_registerName(c"alloc".as_ptr());
            let init_sel = sel_registerName(c"init".as_ptr());

            let obj = send(cls, alloc_sel);
            send(obj, init_sel)
        }
    }

    #[cfg(target_os = "macos")]
    fn release_nsview(view: *mut std::ffi::c_void) {
        unsafe {
            let send = objc_send();
            let release_sel = sel_registerName(c"release".as_ptr());
            send(view, release_sel);
        }
    }
}
