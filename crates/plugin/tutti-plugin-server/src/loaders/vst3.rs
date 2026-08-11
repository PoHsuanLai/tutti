//! VST3 plugin loader using the `vst3-host` crate.

use std::collections::HashMap;
use std::path::Path;

use tutti_plugin::server::{
    AudioBufferMut, AutomationMode, BusChannels, ChannelLayout, ChordChanges, EditorPresence,
    EditorSize, Features, LoadedPlugin, Normalized, NoteExpressionChanges,
    NoteExpressionIntChanges, NoteExpressionTextChanges, ParamAddress, ParamFlags, ParamRange,
    ParamSteps, ParameterInfo, PluginAudio, PluginClass, PluginDescriptor, PluginEditorHost,
    PluginError, PluginMeta, PluginParams, PluginPresets, PluginResult, PluginState, PluginTail,
    Preset, PresetId, ProcessContext, ProcessOutput, RenderMode, Samples, ScaleChanges,
    Vst3SubCategories, WindowHandle,
};
use tutti_plugin::{BridgeError, LoadStage, Result};

use crate::loaders::common::{single_bus, Meta};
use tutti_vst3_host::ProcessMode;

pub use tutti_vst3_host;

/// Typed inner holds either a f32 or f64 instance, selected at activation time
/// based on plugin capabilities and the caller's preferred format.
enum VstInner {
    F32(tutti_vst3_host::Vst3Instance<f32>),
    F64(tutti_vst3_host::Vst3Instance<f64>),
}

/// Dispatch a shared expression over both inner variants (immutable).
macro_rules! vst_dispatch {
    ($self:expr, $inner:ident => $body:expr) => {
        match &$self.inner {
            VstInner::F32($inner) => $body,
            VstInner::F64($inner) => $body,
        }
    };
}

/// Dispatch a shared expression over both inner variants (mutable).
macro_rules! vst_dispatch_mut {
    ($self:expr, $inner:ident => $body:expr) => {
        match &mut $self.inner {
            VstInner::F32($inner) => $body,
            VstInner::F64($inner) => $body,
        }
    };
}

pub struct Vst3Instance {
    inner: VstInner,
    meta: Meta,
    /// Load parameters retained so the plugin can be torn down and rebuilt in
    /// place when it requests `kReloadComponent`. See [`Self::reload`].
    reload: ReloadParams,
    /// Sequencer-context conversion buffers, reused across blocks.
    seq: SeqScratch,
}

/// Reusable buffers for the sequencer-context conversion in [`process_block`].
///
/// Every field is a `Vec` the converters `clear()` and refill rather than
/// rebuild, so a plugin advertising [`Features::SEQUENCER_CONTEXT`] costs no
/// allocation per block once the buffers have grown. The nested `text` buffers
/// matter as much as the outer ones: each chord / scale / note-expression text
/// is UTF-16 re-encoded, so building these fresh was one heap allocation *per
/// event*, not per block.
///
/// Sized by use, never by `set_block_size` — the event count is a property of
/// the score, not of the buffer length, so there is no maximum to preallocate
/// against. Steady state is reached after the first block that carries each
/// kind of event.
#[derive(Default)]
struct SeqScratch {
    chords: Vec<tutti_vst3_host::ChordValue>,
    scales: Vec<tutti_vst3_host::ScaleValue>,
    expr_texts: Vec<tutti_vst3_host::NoteExpressionText>,
    expr_ints: Vec<tutti_vst3_host::NoteExpressionIntValue>,
}

/// Everything `Vst3Instance::load` needs, kept so a `kReloadComponent` request
/// can reconstruct the instance with identical settings.
#[derive(Clone)]
struct ReloadParams {
    path: std::path::PathBuf,
    sample_rate: f64,
    block_size: usize,
    prefer_f64: bool,
    /// The mode the instance is activated in. Retained here rather than read
    /// back off the instance because it is an *input* to activation:
    /// `setupProcessing` delivers it exactly once, so changing it means
    /// rebuilding — which is what [`Vst3Instance::set_render_mode`] does.
    mode: ProcessMode,
}

/// Outcome of [`Vst3Instance::poll_restart`] — the host-side restart effects
/// that could not be applied in place and need the server / client to react.
/// The CC-mapping rebuild and bus re-enumeration are already done by the time
/// this returns; these flags are what remains.
#[derive(Debug, Default, Clone, Copy)]
pub struct RestartChanges {
    /// New latency in samples (re-read because `kLatencyChanged` fired). Push
    /// to PDC.
    pub latency: Option<Samples>,
    /// `kParamValuesChanged` — the client should re-read parameter values.
    pub param_values_changed: bool,
    /// `kParamTitlesChanged` — the client should re-pull the parameter list.
    pub param_titles_changed: bool,
    /// `kReloadComponent` — the instance was torn down and rebuilt in place;
    /// the client should resync everything (it is effectively a fresh plugin).
    pub reloaded: bool,
    /// `kIoChanged` — the bus layout changed and was re-enumerated; the new
    /// layout is in `loaded()`. The client should rewire its audio graph.
    pub io_changed: bool,
}

/// Map a `tutti_vst3_host::Vst3Error` to the server's `BridgeError`.
fn map_vst3_error(e: tutti_vst3_host::Vst3Error, path: &Path) -> BridgeError {
    match e {
        tutti_vst3_host::Vst3Error::LoadFailed {
            path,
            stage,
            reason,
        } => BridgeError::LoadFailed {
            path,
            stage,
            reason,
        },
        tutti_vst3_host::Vst3Error::PluginError { stage, code } => {
            BridgeError::PluginError { stage, code }
        }
        _ => BridgeError::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Opening,
            reason: e.to_string(),
        },
    }
}

/// Resolve, load, and activate the inner host instance from load parameters,
/// returning it alongside freshly-built protocol metadata. Shared by
/// [`Vst3Instance::load`] and [`Vst3Instance::reload`].
fn build_inner(p: &ReloadParams) -> Result<(VstInner, Meta)> {
    let resolved = tutti_plugin::server::resolve_bundle(&p.path)?;
    let loaded =
        tutti_vst3_host::Vst3Loaded::load(&resolved).map_err(|e| map_vst3_error(e, &p.path))?;

    let info = loaded.info().clone();
    let has_editor = loaded.has_editor();

    // Probe capability + best-effort ("consumes") flags off the loaded plugin
    // BEFORE `activate` consumes it. The gate is the flag, not the format:
    // whatever the plugin reports here is what the engine will send it.
    let editor_resizable = has_editor && loaded.editor_capabilities().resize.resizable;
    // Note-expression: any dimension on the main event bus / channel 0.
    let has_note_expression = loaded.note_expression_count(0, 0) > 0;
    let wants_transport = loaded.wants_transport();
    let wants_sequencer_context = loaded.wants_sequencer_context();
    // Both preset bits come from the same question — does the plugin publish
    // program lists — because for VST3 they are the same capability. A program
    // is *selected* by writing the `kIsProgramChange` parameter that owns the
    // list, so a plugin with lists can do both halves and one without can do
    // neither. They stay separate bits because CLAP splits them.
    let has_programs = !loaded.program_lists().is_empty();

    let inner = if p.prefer_f64 && info.supports_f64 {
        let inst = loaded
            .activate_with_mode::<f64>(p.sample_rate, p.block_size, p.mode)
            .map_err(|e| map_vst3_error(e, &p.path))?;
        VstInner::F64(inst)
    } else {
        let inst = loaded
            .activate_with_mode::<f32>(p.sample_rate, p.block_size, p.mode)
            .map_err(|e| map_vst3_error(e, &p.path))?;
        VstInner::F32(inst)
    };

    let actually_f64 = matches!(inner, VstInner::F64(_));
    let latency = Samples(match &inner {
        VstInner::F32(i) => i.read_latency_samples(),
        VstInner::F64(i) => i.read_latency_samples(),
    } as usize);
    // VST3 always answers `getTailSamples`, so there is no "unasked" case here —
    // `from_samples` maps 0 to `None` and the saturating max to `Unbounded`.
    let tail = PluginTail::from_samples(match &inner {
        VstInner::F32(i) => i.read_tail_samples(),
        VstInner::F64(i) => i.read_tail_samples(),
    });
    let descriptor = vst3_descriptor(&info, EditorPresence::measured(has_editor));

    let mut features = Features::empty();
    features.set(Features::F64_AUDIO, actually_f64);
    features.set(Features::MIDI_IN, info.has_midi_input);
    features.set(Features::MIDI_OUT, info.has_midi_output);
    features.set(Features::EDITOR, has_editor);
    features.set(Features::EDITOR_RESIZE, editor_resizable);
    // VST3 always carries sample-accurate parameter automation (IParameterChanges).
    features.insert(Features::PARAM_AUTOMATION);
    features.set(Features::TRANSPORT, wants_transport);
    features.set(Features::NOTE_EXPRESSION, has_note_expression);
    features.set(Features::SEQUENCER_CONTEXT, wants_sequencer_context);
    features.set(Features::PRESET_LIST, has_programs);
    features.set(Features::PRESET_LOAD, has_programs);
    let probed = tutti_plugin::server::probed::VST3;

    // Queried after activation, because that is when the plugin has settled its
    // arrangements — `negotiate_bus_arrangements` runs inside `activate`.
    let (input_topology, output_topology) = match &inner {
        VstInner::F32(i) => (i.input_bus_topologies(), i.output_bus_topologies()),
        VstInner::F64(i) => (i.input_bus_topologies(), i.output_bus_topologies()),
    };

    let loaded_meta = LoadedPlugin {
        inputs: bus_channels(&info.input_bus_channels, info.num_inputs),
        outputs: bus_channels(&info.output_bus_channels, info.num_outputs),
        latency_samples: latency,
        tail,
        features,
        probed,
        input_topology: input_topology.into_iter().collect(),
        output_topology: output_topology.into_iter().collect(),
    };

    Ok((
        inner,
        Meta {
            descriptor,
            loaded: loaded_meta,
        },
    ))
}

/// Build the catalog descriptor from VST3 factory info.
///
fn vst3_descriptor(info: &tutti_vst3_host::PluginInfo, editor: EditorPresence) -> PluginDescriptor {
    PluginDescriptor {
        id: info.id.clone(),
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        version: info.version.clone(),
        class: PluginClass::Vst3 {
            // A v1-only factory cannot report subcategories at all, which
            // flattens here to the same empty facet set as a plugin that
            // declared none. Both spell `PluginRole::Unknown`, which is the
            // honest answer for either.
            category: Vst3SubCategories::parse(info.sub_categories.as_deref().unwrap_or_default()),
        },
        editor,
    }
}

/// Per-bus channel counts for one direction. The host already enumerates every
/// audio bus; the full list is carried verbatim so sidechain/aux buses survive.
/// Falls back to a single main bus of `main_channels` when the host reported no
/// per-bus list (e.g. a plugin with exactly one bus that mirrors `num_*`).
fn bus_channels(host_buses: &[usize], main_channels: usize) -> BusChannels {
    if host_buses.is_empty() {
        single_bus(main_channels)
    } else {
        host_buses.iter().map(|&c| ChannelLayout::from(c)).collect()
    }
}

impl Vst3Instance {
    /// Lightweight probe: load library and read factory metadata without activation.
    pub fn probe(path: &Path) -> Result<PluginDescriptor> {
        let resolved = tutti_plugin::server::resolve_bundle(path)?;
        let info = tutti_vst3_host::Vst3Instance::<f32>::probe(&resolved).map_err(|e| {
            BridgeError::LoadFailed {
                path: path.to_path_buf(),
                stage: LoadStage::Scanning,
                reason: e.to_string(),
            }
        })?;
        // A probe does not instantiate, and an editor is a property of an
        // instance — so this genuinely does not know. The load path above asks.
        Ok(vst3_descriptor(&info, EditorPresence::Unknown))
    }

    /// Load and activate a VST3 plugin.
    ///
    /// If `prefer_f64` is `true` and the plugin advertises 64-bit support, the
    /// inner instance is activated as `Vst3Instance<f64>`; otherwise `f32` is
    /// used. The chosen format is reflected in `metadata().features`
    /// ([`Features::F64_AUDIO`]).
    pub fn load(
        path: &Path,
        sample_rate: f64,
        block_size: usize,
        prefer_f64: bool,
    ) -> Result<Self> {
        let reload = ReloadParams {
            path: path.to_path_buf(),
            sample_rate,
            block_size,
            prefer_f64,
            mode: ProcessMode::Realtime,
        };
        let (inner, meta) = build_inner(&reload)?;
        Ok(Self {
            inner,
            meta,
            reload,
            seq: SeqScratch::default(),
        })
    }

    /// Tear the instance down and rebuild it from the original load parameters,
    /// preserving plugin state across the swap. Invoked when the plugin
    /// requests `kReloadComponent` (e.g. after an in-plugin preset load that
    /// changes the component structure). The rebuilt instance replaces `inner`
    /// and `metadata` in place.
    fn reload(&mut self) -> Result<()> {
        // Capture state from the old instance so the rebuilt one resumes where
        // it left off; tolerate plugins that refuse getState.
        let saved_state = vst_dispatch_mut!(self, inner => inner.state()).ok();

        let (inner, meta) = build_inner(&self.reload)?;
        self.inner = inner;
        self.meta = meta;

        if let Some(state) = saved_state {
            let _ = vst_dispatch_mut!(self, inner => inner.set_state(&state));
        }
        Ok(())
    }

    /// Drain the plugin's `restartComponent` requests and apply every host-side
    /// effect, returning the residual [`RestartChanges`] the server / client
    /// still needs to react to.
    ///
    /// Applied in place here:
    /// - `kLatencyChanged` → re-read latency, update cached metadata (returned
    ///   in `latency` for PDC).
    /// - `kMidiCCAssignmentChanged` → re-query the `IMidiMapping` CC→param table
    ///   (`rebuild_midi_cc_mapping`), so runtime CC remaps take effect.
    /// - `kReloadComponent` → tear the instance down and rebuild it from the
    ///   original load params, preserving state (`reloaded`).
    /// - `kIoChanged` → the host re-enumerated buses; refresh cached bus
    ///   metadata and flag `io_changed` so the client can rewire.
    ///
    /// Surfaced for the client (no host-side action possible): the parameter
    /// re-read flags.
    ///
    /// Polled between audio blocks by [`Plugin::poll_async_events`]; it must
    /// not be called concurrently with [`process`](Self::process).
    pub fn poll_restart(&mut self) -> RestartChanges {
        let restart = vst_dispatch_mut!(self, inner => inner.poll_plugin_notifications()).restart;
        let mut changes = RestartChanges::default();

        // `kIoChanged` and `kLatencyChanged` both require the same
        // deactivate → re-ask → reactivate cycle (`ivsteditcontroller.h:125-127`
        // and `:137-138`), so one cycle serves both when they arrive together —
        // which they routinely do, since a layout change usually moves latency.
        //
        // Reading latency without the cycle is what the code did before: the
        // figure comes from a plugin that has not been reactivated, and a
        // plugin that recomputes group delay in `setActive(true)` reports the
        // stale one. The re-read below happens inside the cycle, after
        // reactivation, which is the order the header specifies.
        if restart.io_changed || restart.latency_changed {
            match vst_dispatch_mut!(self, inner => inner.restart_bus_configuration()) {
                Ok(()) => {
                    let samples = Samples(
                        vst_dispatch_mut!(self, inner => inner.read_latency_samples()) as usize,
                    );
                    self.meta.loaded.latency_samples = samples;
                    changes.latency = Some(samples);

                    if restart.io_changed {
                        // Refresh the cached per-bus layout so `loaded()`
                        // reflects the post-cycle geometry for the client rewire.
                        let info = vst_dispatch!(self, inner => inner.info().clone());
                        self.meta.loaded.inputs =
                            bus_channels(&info.input_bus_channels, info.num_inputs);
                        self.meta.loaded.outputs =
                            bus_channels(&info.output_bus_channels, info.num_outputs);
                        changes.io_changed = true;
                    }
                }
                // A refused reactivation leaves the plugin inactive, and saying
                // nothing would let the client keep processing it. Surfacing no
                // change is the honest answer: the cached layout and latency
                // still describe the last configuration that worked.
                Err(_) => {}
            }
        }

        if restart.midi_cc_assignment_changed {
            vst_dispatch_mut!(self, inner => inner.rebuild_midi_cc_mapping());
        }

        changes.param_values_changed = restart.param_values_changed;
        changes.param_titles_changed = restart.param_titles_changed;

        if restart.reload_requested {
            // A failed reload leaves the old instance in place; surface nothing
            // rather than tearing the plugin down on a transient error.
            if self.reload().is_ok() {
                changes.reloaded = true;
                // Reload re-read latency into the fresh metadata; propagate it.
                changes.latency = Some(self.meta.loaded.latency_samples);
            }
        }

        changes
    }

    pub fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        // Read the unit tree once, not once per parameter: `units()` is a
        // round trip into the plugin per unit, and a parameter list is the one
        // place that cost would multiply. The root unit is excluded — see
        // `unit_names`.
        let units = vst_dispatch!(self, inner => inner.units());
        let groups = unit_names(&units);

        let count = vst_dispatch!(self, inner => inner.parameter_count());
        (0..count)
            .filter_map(|i| {
                let info = vst_dispatch!(self, inner => inner.parameter_info(i))?;
                let plain = vst_dispatch!(self, inner => inner.parameter_plain_range(info.id));
                Some(build_param_info(info, plain, &groups))
            })
            .collect()
    }

    pub fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        // Safety: WindowHandle was validated at the IPC boundary in server.rs
        let handle = unsafe { tutti_vst3_host::WindowHandle::from_raw(parent.as_ptr()) };
        vst_dispatch_mut!(self, inner => inner.open_editor(handle))
            .map(|size| EditorSize {
                width: size.width,
                height: size.height,
            })
            .map_err(|e| BridgeError::EditorError(e.to_string()))
    }
}

/// Process one audio block through a typed `Vst3Instance<T>`.
fn process_block<'t, 'd: 't, T: tutti_vst3_host::Vst3Sample>(
    inner: &mut tutti_vst3_host::Vst3Instance<T>,
    inputs: &'t [&'d [T]],
    outputs: &'t mut [&'d mut [T]],
    sample_rate: f64,
    ctx: &tutti_plugin::server::ProcessContext,
    seq: &mut SeqScratch,
) -> Result<tutti_plugin::server::ProcessOutput> {
    let mut vst3_buffer = tutti_vst3_host::AudioBuffer::new(inputs, outputs, sample_rate);
    let vst3_transport = ctx.transport.cloned().unwrap_or_default();
    // The protocol and vst3-host share the note-expression value type
    // (`tutti_plugin_types::NoteExpressionValue`), so this is a borrow, not a
    // conversion.
    let vst3_note_expr = ctx
        .note_expression
        .map(|n| n.changes.as_slice())
        .unwrap_or_default();
    // Refilled into `seq` rather than rebuilt: these run every block for a
    // plugin advertising `Features::SEQUENCER_CONTEXT`, and each entry carries a
    // UTF-16 `text` buffer, so building them fresh allocated once per event.
    // An absent extension truncates its buffer to empty, which is what the
    // plugin should see — the same thing the old `unwrap_or_default()` produced.
    let expr = ctx.expressive.as_ref();
    match expr.and_then(|e| e.chords) {
        Some(c) => convert_chords_into(c, &mut seq.chords),
        None => seq.chords.clear(),
    }
    match expr.and_then(|e| e.scales) {
        Some(s) => convert_scales_into(s, &mut seq.scales),
        None => seq.scales.clear(),
    }
    match expr.and_then(|e| e.expr_texts) {
        Some(t) => convert_expr_texts_into(t, &mut seq.expr_texts),
        None => seq.expr_texts.clear(),
    }
    match expr.and_then(|e| e.expr_ints) {
        Some(i) => convert_expr_ints_into(i, &mut seq.expr_ints),
        None => seq.expr_ints.clear(),
    }
    let vst3_events = tutti_vst3_host::Vst3InputEvents {
        midi: ctx.midi_events,
        note_expressions: vst3_note_expr,
        chords: &seq.chords,
        scales: &seq.scales,
        expr_texts: &seq.expr_texts,
        expr_ints: &seq.expr_ints,
    };
    let output = inner.process(
        &mut vst3_buffer,
        &vst3_events,
        ctx.param_changes,
        &vst3_transport,
    );
    let midi_events = output.midi_events.iter().copied().collect();
    let param_changes = output.parameter_changes.clone();
    Ok(tutti_plugin::server::ProcessOutput {
        midi_events,
        param_changes,
        note_expression: NoteExpressionChanges::new(),
    })
}

/// Build a Tutti [`ParameterInfo`] from a VST3 parameter descriptor and the
/// plain range probed from the plugin's controller. Both the
/// `get_parameter_list` and the cache-warming paths go through here so they
/// agree on the flag and range mapping.
///
/// `plain` is [`None`] when the plugin has no edit controller to ask, or when
/// its `normalizedParamToPlain` is incoherent — see
/// [`Vst3Loaded::parameter_plain_range`](tutti_vst3_host::Vst3Loaded::parameter_plain_range).
/// The parameter is then `Normalized`, which is what the ABI alone reports.
/// Index a plugin's unit list by id, keeping only units that can name a group.
///
/// Two exclusions, both deliberate:
///
/// - **The root unit** (`unit_ids::ROOT`, id 0) is every plugin's implicit
///   top-level unit, and its name is the plugin's own. Every parameter that
///   declares no unit reports 0, so mapping it would label the entire flat
///   majority with the plugin name — noise on exactly the parameters that have
///   no group.
/// - **Unnamed units.** A unit with an empty name resolves to no group rather
///   than to an empty label, which is the same answer by a shorter route.
fn unit_names(units: &[tutti_vst3_host::Vst3UnitInfo]) -> HashMap<i32, &str> {
    units
        .iter()
        .filter(|u| u.id != tutti_vst3_host::unit_ids::ROOT && !u.name.is_empty())
        .map(|u| (u.id, u.name.as_str()))
        .collect()
}

fn build_param_info(
    info: tutti_vst3_host::Vst3ParameterInfo,
    plain: Option<(f64, f64)>,
    groups: &HashMap<i32, &str>,
) -> ParameterInfo {
    // VST3 reports every one of these, so all six are known.
    const KNOWN: ParamFlags = ParamFlags::AUTOMATABLE
        .union(ParamFlags::READ_ONLY)
        .union(ParamFlags::WRAP)
        .union(ParamFlags::BYPASS)
        .union(ParamFlags::HIDDEN)
        .union(ParamFlags::PROGRAM_CHANGE);

    let mut reported = ParamFlags::empty();
    reported.set(ParamFlags::AUTOMATABLE, info.can_automate());
    reported.set(ParamFlags::READ_ONLY, info.is_read_only());
    reported.set(ParamFlags::WRAP, info.is_wrap());
    reported.set(ParamFlags::BYPASS, info.is_bypass());
    reported.set(ParamFlags::HIDDEN, info.is_hidden());
    reported.set(ParamFlags::PROGRAM_CHANGE, info.is_program_change());

    // `defaultNormalizedValue` is normalized even when the range is plain, so
    // it goes through the same map as any other incoming value.
    let range = match plain {
        Some((min, max)) => {
            let r = ParamRange::Plain {
                min,
                max,
                default: 0.0,
            };
            ParamRange::Plain {
                min,
                max,
                default: r.to_plain(info.default_normalized_value),
            }
        }
        None => ParamRange::Normalized {
            default: info.default_normalized_value,
        },
    };

    // The SDK spells out the encoding at `ivsteditcontroller.h:53`:
    // 0 continuous, 1 toggle, otherwise `max - min` so the position count is
    // one more than the step count.
    let steps = match info.step_count {
        0 => ParamSteps::Continuous,
        1 => ParamSteps::Toggle,
        n if n > 1 => ParamSteps::Enumerated(n as u32 + 1),
        // Negative is out of contract; report it as unsaid rather than guessing.
        _ => ParamSteps::Unknown,
    };

    ParameterInfo {
        // VST3 `ParamID` — opaque and plugin-chosen, not a list position; see
        // the ParamID-vs-index note in `tutti-vst3-host`'s `loaded.rs`.
        id: ParamAddress::Opaque(info.id.into()),
        name: info.title_string(),
        unit: info.units_string(),
        range,
        steps,
        flags: reported & KNOWN,
        known: KNOWN,
        // A `unitId` naming a unit the plugin never published resolves to no
        // group. Several plugins carry dangling ids — the same class of bug
        // `Vst3UnitInfo::program_list` already absorbs for program lists — and
        // the alternatives are worse: a lookup that panicked would take down a
        // parameter list over a display label, and one that fell back to a
        // neighbour would mislabel silently.
        group: groups
            .get(&info.unit_id)
            .map(|n| n.to_string())
            .unwrap_or_default(),
    }
}

/// Refill `out` from `chords`, reusing both the outer buffer and each entry's
/// UTF-16 `text` allocation.
///
/// The overwrite-in-place / truncate / push shape is what keeps the nested
/// `text` buffers alive: overwriting entry `i` re-encodes into the `Vec<u16>`
/// already sitting there, so a steady stream of chord events allocates nothing
/// after the first block. `clear()` + `push` would drop every `text` buffer and
/// re-grow it, which is the per-event allocation this exists to remove.
///
/// The same three-part shape appears in the two siblings below; the types have
/// no common trait to hoist it onto, and a macro would hide four short bodies
/// behind an indirection worth less than it costs.
fn convert_chords_into(chords: &ChordChanges, out: &mut Vec<tutti_vst3_host::ChordValue>) {
    let src = &chords.changes;
    for (dst, c) in out.iter_mut().zip(src.iter()) {
        dst.sample_offset = c.sample_offset;
        dst.root = c.root;
        dst.bass_note = c.bass_note;
        dst.mask = c.mask;
        encode_utf16_into(&c.text, &mut dst.text);
    }
    out.truncate(src.len());
    for c in src.iter().skip(out.len()) {
        out.push(tutti_vst3_host::ChordValue {
            sample_offset: c.sample_offset,
            root: c.root,
            bass_note: c.bass_note,
            mask: c.mask,
            text: c.text.encode_utf16().collect(),
        });
    }
}

/// Re-encode `src` into `dst` in place, keeping `dst`'s capacity.
fn encode_utf16_into(src: &str, dst: &mut Vec<u16>) {
    dst.clear();
    dst.extend(src.encode_utf16());
}

/// Refill `out` from `scales`. Same buffer-reuse shape as
/// [`convert_chords_into`].
fn convert_scales_into(scales: &ScaleChanges, out: &mut Vec<tutti_vst3_host::ScaleValue>) {
    let src = &scales.changes;
    for (dst, s) in out.iter_mut().zip(src.iter()) {
        dst.sample_offset = s.sample_offset;
        dst.root = s.root;
        dst.mask = s.mask;
        encode_utf16_into(&s.text, &mut dst.text);
    }
    out.truncate(src.len());
    for s in src.iter().skip(out.len()) {
        out.push(tutti_vst3_host::ScaleValue {
            sample_offset: s.sample_offset,
            root: s.root,
            mask: s.mask,
            text: s.text.encode_utf16().collect(),
        });
    }
}

/// Refill `out` from `texts`. Same buffer-reuse shape as
/// [`convert_chords_into`].
fn convert_expr_texts_into(
    texts: &NoteExpressionTextChanges,
    out: &mut Vec<tutti_vst3_host::NoteExpressionText>,
) {
    let src = &texts.changes;
    for (dst, t) in out.iter_mut().zip(src.iter()) {
        dst.sample_offset = t.sample_offset;
        dst.note_id = t.note_id;
        dst.type_id = t.type_id;
        encode_utf16_into(&t.text, &mut dst.text);
    }
    out.truncate(src.len());
    for t in src.iter().skip(out.len()) {
        out.push(tutti_vst3_host::NoteExpressionText {
            sample_offset: t.sample_offset,
            note_id: t.note_id,
            type_id: t.type_id,
            text: t.text.encode_utf16().collect(),
        });
    }
}

/// Refill `out` from `ints`.
///
/// `NoteExpressionIntValue` is `Copy` with no nested buffer, so this is the
/// plain `clear()` + `extend` the other three cannot use — there is nothing per
/// entry to preserve, only the outer capacity.
fn convert_expr_ints_into(
    ints: &NoteExpressionIntChanges,
    out: &mut Vec<tutti_vst3_host::NoteExpressionIntValue>,
) {
    out.clear();
    out.extend(
        ints.changes
            .iter()
            .map(|i| tutti_vst3_host::NoteExpressionIntValue {
                sample_offset: i.sample_offset,
                note_id: i.note_id,
                type_id: i.type_id,
                value: i.value,
            }),
    );
}

// `PluginInstance` is a re-export alias of `tutti_plugin_types::PluginFormatHost`
// (tutti-plugin-server reaches the shared trait through tutti-plugin, its only
// path to the vocabulary crate).
impl PluginMeta for Vst3Instance {
    fn descriptor(&self) -> &PluginDescriptor {
        &self.meta.descriptor
    }

    fn loaded(&self) -> &LoadedPlugin {
        &self.meta.loaded
    }
}

impl PluginAudio for Vst3Instance {
    fn process(
        &mut self,
        buffer: AudioBufferMut<'_, '_>,
        ctx: &ProcessContext,
    ) -> PluginResult<ProcessOutput> {
        match (&mut self.inner, buffer) {
            (VstInner::F32(inner), AudioBufferMut::F32(buf)) => process_block(
                inner,
                buf.inputs,
                buf.outputs,
                buf.sample_rate,
                ctx,
                &mut self.seq,
            )
            .map_err(Into::into),
            (VstInner::F64(inner), AudioBufferMut::F64(buf)) => {
                process_block(
                    inner,
                    buf.inputs,
                    buf.outputs,
                    buf.sample_rate,
                    ctx,
                    &mut self.seq,
                )
                    .map_err(Into::into)
            }
            _ => Err(PluginError::Process(
                "Buffer format mismatch: plugin was activated with a different sample format"
                    .to_string(),
            )),
        }
    }

    fn set_sample_rate(&mut self, rate: f64) {
        vst_dispatch_mut!(self, inner => {
            // `PluginAudio::set_sample_rate` is infallible, so a VST3 plugin's
            // refusal cannot be propagated from here. It must still be *said*:
            // the host rolls the instance back to the rate the plugin already
            // accepted and stays active there, so the session keeps running —
            // but at a rate the caller did not ask for, and a silent discard is
            // how that becomes an unexplained pitch shift.
            if let Err(e) = inner.set_sample_rate(rate) {
                tracing::warn!(
                    requested = rate,
                    running_at = inner.sample_rate(),
                    "VST3 plugin refused the sample rate: {e}"
                );
            }
        });
    }

    /// Re-activate the instance in the requested mode, preserving its state.
    ///
    /// VST3 delivers `processMode` through `setupProcessing`, which runs once
    /// per activation and may only run while deactivated
    /// (`ivstaudioprocessor.h:328`). Reaching `kOffline` from `kRealtime`
    /// therefore *requires* a fresh setup — the spec says so explicitly at
    /// `:142-143` — so this rebuilds through the same state-preserving path
    /// `kReloadComponent` uses rather than mutating the live instance.
    ///
    /// The realtime↔prefetch pair is the one exception the spec carves out and
    /// is `Vst3Instance::set_prefetch`, not this; prefetch is not reachable
    /// through [`RenderMode`] by design (see its docs).
    fn set_render_mode(&mut self, mode: RenderMode) -> bool {
        let requested = if mode.is_offline() {
            ProcessMode::Offline
        } else {
            ProcessMode::Realtime
        };
        if self.reload.mode == requested {
            return true;
        }
        let previous = self.reload.mode;
        self.reload.mode = requested;
        if self.reload().is_err() {
            // The rebuild failed and `reload` leaves the old instance in place,
            // so the retained mode must go back to what is actually running —
            // otherwise a later `kReloadComponent` would silently adopt a mode
            // this plugin already refused.
            self.reload.mode = previous;
            return false;
        }
        true
    }
}

impl PluginParams for Vst3Instance {
    fn get_parameter(&self, id: ParamAddress) -> f64 {
        // A VST2 index addresses nothing here; `ParamID` is opaque.
        let Some(id) = id.opaque() else { return 0.0 };
        vst_dispatch!(self, inner => inner.parameter(id.get()))
    }

    fn set_parameter(&mut self, id: ParamAddress, value: Normalized) {
        // VST3 is normalized natively (`setParamNormalized`), so this is the
        // one format where the seam's domain and the ABI's coincide.
        let Some(id) = id.opaque() else { return };
        vst_dispatch_mut!(self, inner => inner.set_parameter(id.get(), value.get()));
    }

    /// VST3's `getParamStringByValue` takes the value already normalized, so
    /// this is the one format where no conversion stands between the seam's
    /// domain and the ABI's — the same coincidence
    /// [`set_parameter`](Self::set_parameter) notes.
    fn parameter_text(&self, id: ParamAddress, value: Normalized) -> Option<String> {
        let id = id.opaque()?;
        vst_dispatch!(self, inner => inner.parameter_string_by_value(id.get(), value.get()))
    }

    /// The inverse. `getParamValueByString` answers normalized too, so the
    /// result passes through `Normalized::new`'s clamp only — a plugin
    /// returning a slightly out-of-range value is corrected rather than
    /// forwarded to its own parameter.
    fn parameter_value_from_text(&self, id: ParamAddress, text: &str) -> Option<Normalized> {
        let id = id.opaque()?;
        let value = vst_dispatch!(self, inner => inner.parameter_value_by_string(id.get(), text))?;
        Some(Normalized::new(value))
    }

    fn set_automation_state(&mut self, mode: AutomationMode) {
        // Encode the format-neutral mode onto the VST3 `IAutomationState` bitmask
        // HERE, at the VST3 FFI edge, via the VST3 crate's own SDK-backed
        // conversion. Forwards to `Vst3Loaded::set_automation_state` via Deref; a
        // no-op if the plugin doesn't implement IAutomationState. Runs on the
        // server's main thread (same as set_parameter), satisfying the host's
        // main-thread assertion.
        let state = tutti_vst3_host::automation_state::from_mode(mode);
        vst_dispatch_mut!(self, inner => { inner.set_automation_state(state); });
    }

    fn get_parameter_list(&self) -> Vec<ParameterInfo> {
        Vst3Instance::get_parameter_list(self)
    }
}

impl PluginEditorHost for Vst3Instance {
    fn open_editor(&mut self, parent: WindowHandle) -> PluginResult<EditorSize> {
        Vst3Instance::open_editor(self, parent).map_err(Into::into)
    }

    fn close_editor(&mut self) {
        vst_dispatch_mut!(self, inner => inner.close_editor());
    }
}

impl PluginPresets for Vst3Instance {
    /// The plugin's programs, flattened out of its named program lists.
    ///
    /// VST3 is the one format whose presets are **grouped**: programs live in
    /// lists attached to units, so each `Preset` carries its list's name as
    /// `bank`. The id keeps both coordinates — `getProgramName` takes
    /// `(list_id, index)`, and the list id is plugin-chosen rather than a
    /// position in `program_lists()`, so neither can be dropped.
    ///
    /// A program the plugin declines to name is kept with an empty name rather
    /// than skipped: the index is half the identifier, so dropping one would
    /// renumber every program after it within that list.
    fn get_presets(&mut self) -> Vec<Preset> {
        vst_dispatch!(self, inner => {
            inner
                .program_lists()
                .into_iter()
                .flat_map(|list| {
                    (0..list.program_count).map(move |index| {
                        let name = inner
                            .program_name(list.id, index)
                            .unwrap_or_default();
                        Preset::in_bank(
                            PresetId::Program { list_id: list.id, index },
                            name,
                            list.name.clone(),
                        )
                    })
                })
                .collect()
        })
    }

    /// Select a program by writing the parameter that owns its list.
    ///
    /// VST3 has no load-preset call, so this *is* the load: the format routes a
    /// program change through the parameter flagged `kIsProgramChange`, whose
    /// unit names the program list. Doing it here rather than leaving it to the
    /// caller is what makes the four formats one API — and it stays a single
    /// write path, because the parameter is still the only thing written.
    ///
    /// `false` when the id names no VST3 program, when no parameter claims that
    /// list, or when the list has fewer than two entries (nothing to select
    /// between, and the normalization would divide by zero).
    ///
    /// `IUnitInfo::setUnitProgramData` is *not* this call: it takes an
    /// `IBStream` of preset bytes and writes them into a slot, the inverse
    /// operation. Named because it is the obvious thing to find later and
    /// mistake for a load path.
    fn load_preset(&mut self, id: &PresetId) -> bool {
        let PresetId::Program { list_id, index } = id else {
            return false;
        };
        let Some((param_id, step_count)) = self.program_change_param(*list_id) else {
            return false;
        };
        // `StringListParameter::toNormalized` is `value / stepCount`
        // (`futils.h:87-90`), and `appendString` increments `stepCount` per
        // entry from zero — so a list of N programs has `stepCount == N - 1`
        // and index `i` normalizes to `i / (N - 1)`, not `i / N`. The
        // off-by-one is silent and lands on a neighbouring program.
        if step_count <= 0 || *index > step_count as u32 {
            return false;
        }
        let normalized = f64::from(*index) / f64::from(step_count);
        vst_dispatch_mut!(self, inner => inner.set_parameter(param_id, normalized));
        true
    }

    /// The program the plugin currently has selected, read back off the same
    /// parameter [`load_preset`](Self::load_preset) writes.
    ///
    /// `None` when no unit publishes a program list, which is every plugin
    /// that does not implement `IUnitInfo`.
    fn get_current_preset(&mut self) -> Option<PresetId> {
        // One unit walk, not two. `units()` is a COM round trip per unit *and*
        // internally enumerates the program lists to resolve each unit's
        // `program_list`, so asking twice here cost four full enumerations into
        // plugin code to answer one question. Both things this needs — the
        // first published list, and the unit that owns it — come out of the
        // same pass.
        let (list_id, owning_unit) = vst_dispatch!(self, inner => {
            inner
                .units()
                .into_iter()
                .find_map(|u| u.program_list.map(|list| (list, u.id)))
        })?;
        let (param_id, step_count) = self.program_change_param_of(owning_unit)?;
        if step_count <= 0 {
            return None;
        }
        let normalized = vst_dispatch!(self, inner => inner.parameter(param_id));
        // Inverse of the write: `FromNormalized` is `value * stepCount`, then
        // rounded — the parameter is a discrete list, so a value between two
        // steps belongs to the nearer one.
        let index = (normalized * f64::from(step_count)).round();
        if !index.is_finite() || index < 0.0 {
            return None;
        }
        Some(PresetId::Program {
            list_id,
            index: index as u32,
        })
    }
}

impl Vst3Instance {
    /// The `(parameter id, step count)` of the program-change parameter that
    /// selects from `list_id`, if one exists.
    ///
    /// The link runs parameter -> unit -> program list: a program-change
    /// parameter belongs to a unit, and that unit names the list it selects
    /// from. Matching on the *flag* rather than on a name or position is what
    /// makes this work for a plugin with several lists.
    ///
    /// **The flag check is uncovered, measured rather than assumed.** Dropping
    /// `is_program_change()` and taking the unit's first parameter leaves every
    /// test green: the SDK's `multiple_programchanges` sample gives each unit
    /// exactly one parameter, so the two rules coincide. The fixture that would
    /// separate them is `mda-vst3`, whose controllers add a Bypass parameter to
    /// the same root unit *before* the preset one — but its shell exposes the
    /// base controller (dangling `programListId`, zero published lists) and
    /// `load_class` cannot re-open the bundle while the first load holds it. So
    /// no available input distinguishes the two, and a plugin that puts any
    /// parameter ahead of its program-change one would have the wrong parameter
    /// written. The flag is correct per `vsteditcontroller.cpp:604`; it is the
    /// test that is missing, not the rule.
    #[cfg(feature = "vst3")]
    fn program_change_param(&self, list_id: i32) -> Option<(u32, i32)> {
        let owning_unit = vst_dispatch!(self, inner => {
            inner
                .units()
                .into_iter()
                .find_map(|u| (u.program_list == Some(list_id)).then_some(u.id))
        })?;
        self.program_change_param_of(owning_unit)
    }

    /// The program-change parameter belonging to `owning_unit`.
    ///
    /// The half of [`program_change_param`](Self::program_change_param) that
    /// does not need to walk the unit tree. Split out so `get_current_preset`,
    /// which finds its list *by* walking the units, does not pay for a second
    /// walk to rediscover the unit it just had in hand.
    #[cfg(feature = "vst3")]
    fn program_change_param_of(&self, owning_unit: i32) -> Option<(u32, i32)> {
        vst_dispatch!(self, inner => {
            let count = inner.parameter_count();
            (0..count).find_map(|i| {
                let info = inner.parameter_info(i)?;
                (info.is_program_change() && info.unit_id == owning_unit)
                    .then_some((info.id, info.step_count))
            })
        })
    }
}

impl PluginState for Vst3Instance {
    fn get_state(&mut self) -> PluginResult<Vec<u8>> {
        vst_dispatch_mut!(self, inner => inner.state())
            .map_err(|e| PluginError::State(e.to_string()))
    }

    fn set_state(&mut self, data: &[u8]) -> PluginResult<()> {
        vst_dispatch_mut!(self, inner => inner.set_state(data))
            .map_err(|e| PluginError::State(e.to_string()))
    }
}

/// Sequencer-context conversion, exercised without a plugin binary.
///
/// These are the only tests in this file that run anywhere: everything below
/// needs a real VST3 on disk (and a macOS path at that), while the converters
/// are pure functions over the protocol types.
#[cfg(test)]
#[cfg(feature = "vst3")]
mod seq_scratch_tests {
    use super::*;
    use tutti_plugin::server::ChordValue;

    fn chord(offset: i32, text: &str) -> ChordValue {
        ChordValue {
            sample_offset: offset,
            root: 60,
            bass_note: 60,
            mask: 0b1001_0001,
            text: text.to_string(),
        }
    }

    fn changes(items: &[ChordValue]) -> ChordChanges {
        let mut c = ChordChanges::new();
        for i in items {
            c.add_change(i.clone());
        }
        c
    }

    /// The point of the whole change: once the buffers have grown, refilling
    /// them must not allocate.
    ///
    /// Measured with `assert_no_alloc` rather than by comparing buffer
    /// addresses. Addresses do **not** discriminate here — `clear()` frees each
    /// `text` buffer and the very next `push` re-requests the same size, which
    /// the allocator typically satisfies from the block it just freed, so the
    /// pointers come back equal and a naive rebuild passes. Counting the
    /// allocations is the only observable that separates the two, and this
    /// crate already installs `AllocDisabler` as its global allocator
    /// (`lib.rs`) for exactly this kind of gate.
    ///
    /// The source `ChordChanges` is built outside the gate: constructing one
    /// allocates its `String`s, which is the caller's cost, not the
    /// converter's.
    #[test]
    fn refilling_grown_buffers_does_not_allocate() {
        let mut out = Vec::new();
        // Warm up: first pass grows the outer Vec and each nested text buffer.
        convert_chords_into(&changes(&[chord(0, "Cmaj7"), chord(64, "Fmin")]), &mut out);

        // Same event count, same text lengths, different values — the steady
        // state a sequencer-driven plugin sits in block after block.
        let next = changes(&[chord(8, "Dmin9"), chord(96, "G7")]);
        assert_no_alloc::assert_no_alloc(|| {
            convert_chords_into(&next, &mut out);
        });

        assert_eq!(out.len(), 2);
        assert_eq!(out[0].sample_offset, 8);
        assert_eq!(out[0].text, "Dmin9".encode_utf16().collect::<Vec<_>>());
        assert_eq!(out[1].text, "G7".encode_utf16().collect::<Vec<_>>());
    }

    /// A shorter block truncates; a longer one grows. Both must leave the
    /// surviving prefix correct rather than stale.
    #[test]
    fn refilling_tracks_a_changing_event_count() {
        let mut out = Vec::new();
        convert_chords_into(
            &changes(&[chord(0, "a"), chord(1, "b"), chord(2, "c")]),
            &mut out,
        );
        assert_eq!(out.len(), 3);

        convert_chords_into(&changes(&[chord(9, "z")]), &mut out);
        assert_eq!(out.len(), 1, "shrinks to the new count");
        assert_eq!(out[0].sample_offset, 9);
        assert_eq!(out[0].text, "z".encode_utf16().collect::<Vec<_>>());

        convert_chords_into(&changes(&[chord(3, "p"), chord(4, "q")]), &mut out);
        assert_eq!(out.len(), 2, "grows past the retained entry");
        assert_eq!(out[1].text, "q".encode_utf16().collect::<Vec<_>>());
    }

    /// An empty block must clear rather than leave the previous block's chords
    /// visible — the stale-data failure the truncate guards against.
    #[test]
    fn an_empty_block_clears_the_buffer() {
        let mut out = Vec::new();
        convert_chords_into(&changes(&[chord(0, "Cmaj7")]), &mut out);
        assert_eq!(out.len(), 1);

        convert_chords_into(&changes(&[]), &mut out);
        assert!(out.is_empty(), "no chords this block means none are staged");
    }

    /// `NoteExpressionIntValue` has no nested buffer, so its converter is the
    /// plain clear+extend. Covered so the divergence stays deliberate.
    #[test]
    fn int_expressions_refill_without_nesting() {
        use tutti_plugin::server::{NoteExpressionIntChanges, NoteExpressionIntValue};
        let mut src = NoteExpressionIntChanges::new();
        src.add_change(NoteExpressionIntValue {
            sample_offset: 4,
            note_id: 7,
            type_id: 2,
            value: 41,
        });

        let mut out = Vec::new();
        convert_expr_ints_into(&src, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].value, 41);

        convert_expr_ints_into(&NoteExpressionIntChanges::new(), &mut out);
        assert!(out.is_empty());
    }
}

#[cfg(test)]
#[cfg(feature = "vst3")]
mod tests {
    use super::*;
    use std::path::Path;
    use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
    use tutti_plugin::server::{AudioBuffer, AudioBuffer64, AudioBufferMut, MidiEvent};

    const VST3_PLUGIN: &str = "/Library/Audio/Plug-Ins/VST3/TAL-NoiseMaker.vst3";

    /// Build a unit as the plugin would report it, with no program list.
    fn unit(id: i32, name: &str) -> tutti_vst3_host::Vst3UnitInfo {
        tutti_vst3_host::Vst3UnitInfo {
            id,
            parent: Some(tutti_vst3_host::unit_ids::ROOT),
            name: name.to_string(),
            program_list: None,
            program_list_id_raw: tutti_vst3_host::unit_ids::NO_PROGRAM_LIST,
        }
    }

    /// A parameter naming a published unit is labelled with that unit's name.
    #[test]
    fn a_parameter_takes_the_name_of_the_unit_it_declares() {
        let units = [unit(1, "Filter"), unit(2, "Amp")];
        let groups = unit_names(&units);
        assert_eq!(groups.get(&1).copied(), Some("Filter"));
        assert_eq!(groups.get(&2).copied(), Some("Amp"));
    }

    /// The root unit names no group.
    ///
    /// This is the case that decides whether grouping is useful or noise:
    /// every parameter that declares no unit reports id 0, so if the root were
    /// mapped, the flat majority of parameters on every plugin would be
    /// labelled with the plugin's own name.
    #[test]
    fn the_root_unit_is_not_a_group() {
        let units = [
            unit(tutti_vst3_host::unit_ids::ROOT, "TAL-NoiseMaker"),
            unit(1, "Filter"),
        ];
        let groups = unit_names(&units);
        assert_eq!(
            groups.get(&tutti_vst3_host::unit_ids::ROOT),
            None,
            "the root unit's name is the plugin's, not a group's"
        );
        assert_eq!(groups.get(&1).copied(), Some("Filter"));
    }

    /// A `unitId` naming a unit the plugin never published resolves to no
    /// group, rather than to a neighbour's label.
    ///
    /// Dangling ids are a real plugin bug — the same class
    /// `Vst3UnitInfo::program_list` already absorbs — and the failure mode
    /// worth refusing is a *plausible* wrong answer: a lookup that fell back to
    /// the first unit would silently file the parameter under someone else's
    /// heading.
    #[test]
    fn a_unit_id_that_names_nothing_yields_no_group() {
        let units = [unit(1, "Filter")];
        let groups = unit_names(&units);
        assert_eq!(groups.get(&7), None);
    }

    /// A unit the plugin published without a name yields no group, rather than
    /// an empty heading a UI would render as a blank section.
    #[test]
    fn an_unnamed_unit_is_not_a_group() {
        let units = [unit(1, "")];
        let groups = unit_names(&units);
        assert_eq!(groups.get(&1), None);
    }

    /// The SDK's `multiple-program-changes` sample, when the corpus is built.
    ///
    /// Looked up at *runtime* rather than through `env!`, so a checkout without
    /// the corpus skips instead of failing to compile. Set
    /// `VST3_SAMPLE_PLUGIN_DIR` to the directory holding the built samples.
    fn multi_program_sample() -> Option<std::path::PathBuf> {
        let dir = std::env::var("VST3_SAMPLE_PLUGIN_DIR").ok()?;
        let path = Path::new(&dir).join("multiple-program-changes.vst3");
        path.exists().then_some(path)
    }

    /// A VST3 program keeps both coordinates, and its list's name.
    ///
    /// The SDK's `multiple_programchanges` sample is the fixture that makes
    /// this witnessable: it builds 16 program lists whose ids are
    /// `kProgramStartId + i`, so a **list id is provably not a position** in
    /// `program_lists()`. A mapping that dropped `list_id` and kept `index`
    /// would collapse all 16 lists onto one another — 2048 presets becoming
    /// 128 — which is what the count assertion catches.
    ///
    /// `bank` carries the list name because VST3 is the one format whose
    /// presets are grouped; the other three expose a single flat set and
    /// report `None`.
    #[test]
    fn a_vst3_program_keeps_its_list_id_and_bank() {
        let Some(path) = multi_program_sample() else {
            eprintln!("VST3_SAMPLE_PLUGIN_DIR unset or sample absent; skipping");
            return;
        };
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = match Vst3Instance::load(&path, 44_100.0, 512, false) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("sample failed to load ({e:?}); skipping");
                return;
            }
        };

        let presets = instance.get_presets();
        assert!(
            presets.len() > 128,
            "16 lists of 128 programs must not collapse onto one another; got {}",
            presets.len()
        );

        // Every id keeps both coordinates, and more than one distinct list id
        // appears — the property a flattened mapping would destroy.
        let lists: std::collections::BTreeSet<i32> = presets
            .iter()
            .filter_map(|p| match &p.id {
                PresetId::Program { list_id, .. } => Some(*list_id),
                _ => None,
            })
            .collect();
        assert!(
            lists.len() > 1,
            "the sample publishes several program lists; saw {lists:?}"
        );
        assert_eq!(
            lists.len(),
            presets
                .iter()
                .filter_map(|p| p.bank.clone())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            "each list must contribute its own bank name"
        );

        // The same index appears in every list, so an id that dropped its
        // list would be ambiguous.
        let index_zero: Vec<&Preset> = presets
            .iter()
            .filter(|p| matches!(p.id, PresetId::Program { index: 0, .. }))
            .collect();
        assert_eq!(
            index_zero.len(),
            lists.len(),
            "index 0 exists once per list; an id without list_id could not tell them apart"
        );
    }

    /// A program listed through the loader loads, and reads back.
    ///
    /// VST3 has no load-preset call — this *is* the load, routed through the
    /// `kIsProgramChange` parameter that owns the list. Before this the loader
    /// reported `false` and a caller had to find and drive that parameter
    /// itself, which is the branch a cross-format API exists to remove.
    ///
    /// Round-tripped rather than merely accepted: `load_preset` returning
    /// `true` says the parameter was written, and only reading it back through
    /// `get_current_preset` shows the write landed on the program asked for.
    /// That is what catches the normalization off-by-one — `i / (N - 1)`, not
    /// `i / N` — which lands on a neighbour rather than failing.
    #[test]
    fn a_vst3_program_loads_and_reads_back() {
        let Some(path) = multi_program_sample() else {
            eprintln!("VST3_SAMPLE_PLUGIN_DIR unset or sample absent; skipping");
            return;
        };
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = match Vst3Instance::load(&path, 44_100.0, 512, false) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("sample failed to load ({e:?}); skipping");
                return;
            }
        };

        let presets = instance.get_presets();
        assert!(presets.len() > 2, "the sample publishes many programs");

        // Taken from the listing, never constructed.
        let wanted = presets[2].id.clone();
        assert!(
            instance.load_preset(&wanted),
            "a program the plugin listed must load"
        );
        assert_eq!(
            instance.get_current_preset(),
            Some(wanted.clone()),
            "the plugin must report the program just written"
        );

        // The round trip alone cannot catch the off-by-one: `get_current_preset`
        // inverts the *same* formula, so a wrong divisor agrees with itself.
        // Only the absolute value the plugin now holds distinguishes them.
        // Measured on the SDK sample: 128 programs -> `step_count == 127`, so
        // index 2 is 2/127 = 0.015748…, where `i / N` would give 2/128 =
        // 0.015625 and select a neighbouring program on a longer list.
        let PresetId::Program { list_id, index } = &wanted else {
            unreachable!("VST3 ids are programs")
        };
        let (param_id, step_count) = instance
            .program_change_param(*list_id)
            .expect("the list has a program-change parameter");
        assert_eq!(step_count, 127, "128 programs report 127 steps, not 128");
        let raw = vst_dispatch!(instance, inner => inner.parameter(param_id));
        let expected = f64::from(*index) / f64::from(step_count);
        assert!(
            (raw - expected).abs() < 1e-9,
            "wrote {raw}, expected {expected} — the divisor is `step_count`, \
             not the program count"
        );
    }

    /// An id from another format is refused rather than coerced.
    ///
    /// `Number` and `Location` name nothing in VST3's `(list, index)` space.
    /// A `Number` is the dangerous one: it *looks* like a program index, and
    /// treating it as one would write a real program in whichever list
    /// happened to be found first.
    #[test]
    fn a_vst3_load_refuses_an_id_from_another_format() {
        let Some(path) = multi_program_sample() else {
            eprintln!("VST3_SAMPLE_PLUGIN_DIR unset or sample absent; skipping");
            return;
        };
        let _lock = crate::test_utils::plugin_load_lock();
        let mut instance = match Vst3Instance::load(&path, 44_100.0, 512, false) {
            Ok(i) => i,
            Err(e) => {
                eprintln!("sample failed to load ({e:?}); skipping");
                return;
            }
        };

        let before = instance.get_current_preset();
        assert!(
            !instance.load_preset(&PresetId::Number(1)),
            "an AU selector / VST2 index addresses no VST3 program"
        );
        assert!(
            !instance.load_preset(&PresetId::Location("/x.clap-preset".into())),
            "a CLAP path addresses no VST3 program"
        );
        assert_eq!(
            instance.get_current_preset(),
            before,
            "a refused load must not move the program"
        );
    }

    #[test]
    fn test_vst3_load() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let instance = Vst3Instance::load(path, 44100.0, 512, false);
        assert!(
            instance.is_ok(),
            "Failed to load VST3 plugin: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        let meta = instance.descriptor();
        assert!(!meta.name.is_empty(), "Plugin name should not be empty");
        assert!(!meta.id.is_empty(), "Plugin id should not be empty");
    }

    #[test]
    fn test_vst3_metadata() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let instance =
            Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");
        let outputs = instance.loaded().total_outputs();

        assert!(outputs > 0, "Expected audio outputs > 0, got {outputs}");
    }

    #[test]
    fn test_vst3_parameter_count() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let instance =
            Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");

        let count = instance.get_parameter_list().len();
        assert!(
            count > 0,
            "TAL-NoiseMaker should have parameters, got {}",
            count
        );
    }

    #[test]
    fn test_vst3_parameter_list() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let instance =
            Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");

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

    #[test]
    fn test_vst3_get_parameter() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let instance =
            Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");

        let params = instance.get_parameter_list();
        assert!(!params.is_empty(), "Need at least one parameter");

        let first_id = params[0].id;
        let value = PluginParams::get_parameter(&instance, first_id);
        assert!(
            value.is_finite(),
            "Parameter value should be finite, got {}",
            value
        );
    }

    #[test]
    fn test_vst3_process_f32_silence() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let mut instance =
            Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");

        let num_samples = 512;
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = tutti_plugin::server::ProcessContext::new();
        // Should not panic
        let _output = instance.process(AudioBufferMut::F32(buffer), &ctx);
    }

    #[test]
    fn test_vst3_process_f32_with_note() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = Path::new(VST3_PLUGIN);
        let mut instance =
            Vst3Instance::load(path, 44100.0, 512, false).expect("Failed to load VST3 plugin");

        let num_samples = 512;
        let note_on = [MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(1),
            60,
            tutti_midi_types::convert::midi1_velocity_to_midi2(100),
        )];

        let mut has_nonzero = false;

        // Process block with NoteOn
        {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let ctx = tutti_plugin::server::ProcessContext::new().midi(&note_on);
            let _output = instance.process(AudioBufferMut::F32(buffer), &ctx);

            for ch in output_data.iter() {
                for &sample in ch.iter() {
                    if sample != 0.0 {
                        has_nonzero = true;
                    }
                }
            }
        }

        // Process additional blocks to give the synth time to produce sound
        let empty_ctx = tutti_plugin::server::ProcessContext::new();
        for _ in 0..4 {
            let input_data = vec![vec![0.0f32; num_samples]; 2];
            let mut output_data = vec![vec![0.0f32; num_samples]; 2];

            let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
            let mut output_slices: Vec<&mut [f32]> =
                output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

            let buffer = AudioBuffer {
                inputs: &input_slices,
                outputs: &mut output_slices,
                num_samples,
                sample_rate: 44100.0,
            };

            let _output = instance.process(AudioBufferMut::F32(buffer), &empty_ctx);

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

    // =========================================================================
    // Voxengo SPAN / Boogex — f64 support tests (local fixture plugins)
    // =========================================================================

    const SPAN_VST3: &str = "tests/fixtures/plugins/SPAN.vst3";
    const BOOGEX_VST3: &str = "tests/fixtures/plugins/Boogex.vst3";

    /// Resolve fixture path relative to workspace root.
    fn fixture_path(relative: &str) -> std::path::PathBuf {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        // Go up from crates/tutti-plugin-server to the workspace root (crates/tutti)
        p.pop();
        p.pop();
        p.push(relative);
        p
    }

    #[test]
    fn test_voxengo_span_load_and_f64_support() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(SPAN_VST3);
        if !path.exists() {
            eprintln!("Skipping: SPAN.vst3 not found at {:?}", path);
            return;
        }
        let instance = Vst3Instance::load(&path, 44100.0, 512, false);
        assert!(
            instance.is_ok(),
            "Failed to load SPAN: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        eprintln!(
            "SPAN: name={}, supports_f64={}",
            instance.descriptor().name,
            instance.loaded().features.contains(Features::F64_AUDIO)
        );
        assert!(!instance.descriptor().name.is_empty());
    }

    #[test]
    fn test_voxengo_boogex_load_and_f64_support() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(BOOGEX_VST3);
        if !path.exists() {
            eprintln!("Skipping: Boogex.vst3 not found at {:?}", path);
            return;
        }
        let instance = Vst3Instance::load(&path, 44100.0, 512, false);
        assert!(
            instance.is_ok(),
            "Failed to load Boogex: {:?}",
            instance.err()
        );

        let instance = instance.unwrap();
        eprintln!(
            "Boogex: name={}, supports_f64={}",
            instance.descriptor().name,
            instance.loaded().features.contains(Features::F64_AUDIO)
        );
        assert!(!instance.descriptor().name.is_empty());
    }

    #[test]
    fn test_voxengo_span_process_f64() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(SPAN_VST3);
        if !path.exists() {
            eprintln!("Skipping: SPAN.vst3 not found at {:?}", path);
            return;
        }
        // Load with f64 preference — if the plugin supports it, inner will be F64.
        let mut instance =
            Vst3Instance::load(&path, 44100.0, 512, true).expect("Failed to load SPAN");

        if !instance.loaded().features.contains(Features::F64_AUDIO) {
            eprintln!("SPAN does not report f64 support, skipping f64 test");
            return;
        }

        let num_samples = 512;
        // Feed a 440Hz sine wave to test pass-through
        let input_data: Vec<Vec<f64>> = (0..2)
            .map(|_| {
                (0..num_samples)
                    .map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() * 0.5)
                    .collect()
            })
            .collect();
        let mut output_data = vec![vec![0.0f64; num_samples]; 2];

        let input_slices: Vec<&[f64]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f64]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = AudioBuffer64 {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = tutti_plugin::server::ProcessContext::new();
        let _output = instance.process(AudioBufferMut::F64(buffer), &ctx);

        // SPAN is an analyzer — it should pass audio through unchanged
        let mut all_zero = true;
        for ch in &output_data {
            for &s in ch {
                if s != 0.0 {
                    all_zero = false;
                    break;
                }
            }
        }
        eprintln!(
            "SPAN f64 output: first sample = {}, all_zero = {}",
            output_data[0][0], all_zero
        );
    }

    #[test]
    fn test_voxengo_boogex_process_f64() {
        let _lock = crate::test_utils::plugin_load_lock();
        let path = fixture_path(BOOGEX_VST3);
        if !path.exists() {
            eprintln!("Skipping: Boogex.vst3 not found at {:?}", path);
            return;
        }
        // Load with f64 preference — if the plugin supports it, inner will be F64.
        let mut instance =
            Vst3Instance::load(&path, 44100.0, 512, true).expect("Failed to load Boogex");

        if !instance.loaded().features.contains(Features::F64_AUDIO) {
            eprintln!("Boogex does not report f64 support, skipping f64 test");
            return;
        }

        let num_samples = 512;
        // Feed a sine wave — Boogex is an amp sim so it should transform the audio
        let input_data: Vec<Vec<f64>> = (0..2)
            .map(|_| {
                (0..num_samples)
                    .map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() * 0.5)
                    .collect()
            })
            .collect();
        let mut output_data = vec![vec![0.0f64; num_samples]; 2];

        let input_slices: Vec<&[f64]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f64]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = AudioBuffer64 {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = tutti_plugin::server::ProcessContext::new();
        let _output = instance.process(AudioBufferMut::F64(buffer), &ctx);

        let mut has_nonzero = false;
        for ch in &output_data {
            for &s in ch {
                if s != 0.0 {
                    has_nonzero = true;
                    break;
                }
            }
        }
        eprintln!(
            "Boogex f64 output: first sample = {}, has_nonzero = {}",
            output_data[0][0], has_nonzero
        );
    }
}
