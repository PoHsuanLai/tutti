//! The control surface of a VST3 plugin in the shared vocabulary, implemented
//! by [`Vst3Loaded`] and, with parameter writes redirected, by
//! [`Vst3Controller`](super::Vst3Controller).

use tutti_plugin_types::MainThread;
use tutti_plugin_types::features::probed;
use tutti_plugin_types::{
    AutomationMode, BusChannels, ContentScale, Control, EditorPresence, EditorSize, EditorWindow,
    Features, LoadedPlugin, Normalized, ParamId, ParameterInfo, PluginClass, PluginDescriptor,
    PluginError, PluginResult, PluginTail, Preset, PresetId, RenderMode, Request, RescanScope,
    Samples, Vst3SubCategories, WindowHandle,
};

use crate::error::Vst3Error;
use crate::types::PluginInfo;

use super::instance::bus_layout;
use super::loaded::Vst3Loaded;

/// A `Vst3Error` as the shared vocabulary's error.
pub(crate) fn plugin_error(error: Vst3Error) -> PluginError {
    match error {
        Vst3Error::LoadFailed { stage, cause, .. } => PluginError::Load {
            stage,
            reason: cause.to_string(),
        },
        Vst3Error::PluginError { stage, code } => PluginError::Load {
            stage,
            reason: format!("the plugin returned code {code}"),
        },
        Vst3Error::StateError(reason) => PluginError::State(reason),
        Vst3Error::EditorError(reason) => {
            PluginError::Editor(tutti_plugin_types::EditorError::PluginError(reason))
        }
        other => PluginError::Other(other.to_string()),
    }
}

/// The catalog descriptor from VST3 factory info.
///
/// A v1-only factory cannot report subcategories at all, which flattens here
/// to the same empty facet set as a plugin that declared none: both spell
/// `PluginRole::Unknown`, which is the honest answer for either.
#[must_use]
pub fn descriptor_of(info: &PluginInfo, editor: EditorPresence) -> PluginDescriptor {
    PluginDescriptor {
        id: info.id.clone(),
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        version: info.version.clone(),
        class: PluginClass::Vst3 {
            category: Vst3SubCategories::parse(info.sub_categories.as_deref().unwrap_or_default()),
        },
        editor,
    }
}

/// Per-bus channel counts for one direction. The host already enumerates every
/// audio bus; the full list is carried verbatim so sidechain and aux buses
/// survive. Falls back to a single main bus of `main_channels` when the host
/// reported no per-bus list.
fn bus_channels(host_buses: &[usize], main_channels: usize) -> BusChannels {
    if host_buses.is_empty() {
        vec![bus_layout(main_channels)]
    } else {
        host_buses.iter().map(|&c| bus_layout(c)).collect()
    }
}

impl Vst3Loaded {
    /// [`Control::save_state`], with `undrained` stored beside the carried
    /// writes: the Controller's, which no block has taken.
    pub(super) fn save_state_with(
        &mut self,
        main: &MainThread,
        undrained: &[(ParamId, Normalized)],
    ) -> PluginResult<Vec<u8>> {
        self.read_state(main, undrained).map_err(plugin_error)
    }

    /// [`Control::load_state`], keeping the writes the blob carried: applied to
    /// the controller already, and owed to the component's next block.
    pub(super) fn restore_state(
        &mut self,
        main: &MainThread,
        bytes: &[u8],
    ) -> PluginResult<Vec<(ParamId, Normalized)>> {
        self.carried.clear();
        self.write_state(main, bytes).map_err(plugin_error)
    }

    /// The descriptor for this plugin as loaded: its factory info, with whether
    /// it has an editor measured from the plugin itself.
    pub(super) fn measured_descriptor(&self, main: &MainThread) -> PluginDescriptor {
        descriptor_of(&self.info, EditorPresence::measured(self.has_editor(main)))
    }

    /// The capabilities this plugin has, as the shared vocabulary names them.
    fn features(&self, main: &MainThread) -> Features {
        let has_editor = self.has_editor(main);
        let mut features = Features::empty();
        // 64-bit only while activated as such: the session's format and the
        // activation agree.
        features.set(Features::F64_AUDIO, self.active.is_f64());
        features.set(Features::MIDI_IN, self.info.has_midi_input);
        features.set(Features::MIDI_OUT, self.info.has_midi_output);
        features.set(Features::EDITOR, has_editor);
        features.set(
            Features::EDITOR_RESIZE,
            has_editor && self.editor_capabilities(main).resize.resizable,
        );
        // VST3 always carries sample-accurate parameter automation
        // (IParameterChanges).
        features.insert(Features::PARAM_AUTOMATION);
        features.set(Features::TRANSPORT, self.wants_transport());
        // Note expression: any dimension on the main event bus, channel 0.
        features.set(
            Features::NOTE_EXPRESSION,
            self.note_expression_count(main, 0, 0) > 0,
        );
        features.set(Features::SEQUENCER_CONTEXT, self.wants_sequencer_context());
        // Both preset bits come from the same question, whether the plugin
        // publishes program lists, because for VST3 they are one capability: a
        // program is *selected* by writing the parameter that owns the list.
        let has_programs = !self.program_lists(main).is_empty();
        features.set(Features::PRESET_LIST, has_programs);
        features.set(Features::PRESET_LOAD, has_programs);
        features
    }

    /// The tail figure. VST3 always answers `getTailSamples`, so there is no
    /// "unasked" case: `from_vst3_samples` maps 0 to `None` and the saturating
    /// maximum to `Unbounded`.
    fn tail_figure(&self, main: &MainThread) -> PluginTail {
        PluginTail::from_vst3_samples(self.tail_samples(main))
    }

    /// The `(parameter id, step count)` of the program-change parameter that
    /// selects from `list_id`, if one exists.
    ///
    /// The link runs parameter -> unit -> program list: a program-change
    /// parameter belongs to a unit, and that unit names the list it selects
    /// from. Matching on the *flag* rather than on a name or position is what
    /// makes this work for a plugin with several lists.
    ///
    /// The flag is `kIsProgramChange` (`vsteditcontroller.cpp:604`), not the
    /// unit's first parameter: a unit may carry a bypass parameter before the
    /// preset one. No available fixture separates the two rules, so the flag
    /// check is not covered by a test.
    #[must_use]
    pub fn program_change_parameter(
        &self,
        main: &MainThread,
        list_id: i32,
    ) -> Option<(ParamId, i32)> {
        let owning_unit = self
            .units(main)
            .into_iter()
            .find_map(|u| (u.program_list == Some(list_id)).then_some(u.id))?;
        self.program_change_parameter_of(main, owning_unit)
    }

    /// The program-change parameter belonging to `owning_unit`: the half of
    /// [`program_change_parameter`](Self::program_change_parameter) that does
    /// not walk the unit tree, for a caller that already holds the unit.
    fn program_change_parameter_of(
        &self,
        main: &MainThread,
        owning_unit: crate::UnitId,
    ) -> Option<(ParamId, i32)> {
        (0..self.parameter_count(main)).find_map(|i| {
            let info = self.parameter_info_at(main, i)?;
            (info.is_program_change() && info.unit_id() == owning_unit)
                .then_some((ParamId::new(info.id()), info.step_count()))
        })
    }

    /// The parameter write that selects the program `id` names: VST3 has no
    /// load-preset call, so this *is* the load, routed through the parameter
    /// flagged `kIsProgramChange` that owns the list.
    ///
    /// `None` when `id` names no VST3 program, when no parameter claims that
    /// list, or when the list has fewer than two entries (nothing to select
    /// between, and the normalization would divide by zero).
    ///
    /// `IUnitInfo::setUnitProgramData` is *not* this call: it takes an
    /// `IBStream` of preset bytes and writes them into a slot, the inverse
    /// operation.
    pub(super) fn program_write(
        &self,
        main: &MainThread,
        id: &PresetId,
    ) -> Option<(ParamId, Normalized)> {
        let PresetId::Program { list_id, index } = id else {
            return None;
        };
        let (param_id, step_count) = self.program_change_parameter(main, *list_id)?;
        // `StringListParameter::toNormalized` is `value / stepCount`
        // (`futils.h:87-90`), and `appendString` increments `stepCount` per
        // entry from zero, so a list of N programs has `stepCount == N - 1` and
        // index `i` normalizes to `i / (N - 1)`, not `i / N`. The off-by-one is
        // silent and lands on a neighbouring program.
        if step_count <= 0 || *index > step_count as u32 {
            return None;
        }
        Some((
            param_id,
            Normalized::clamped(f64::from(*index) / f64::from(step_count)),
        ))
    }

    /// `setParamNormalized` on the edit controller, for the Loaded state and
    /// the Controller's immediate half alike. `false` when the plugin has no
    /// controller.
    pub(super) fn write_normalized(
        &self,
        main: &MainThread,
        id: ParamId,
        value: Normalized,
    ) -> bool {
        self.plugin.set_parameter_normalized(main, id, value.get())
    }
}

impl Control for Vst3Loaded {
    fn info(&self) -> &PluginDescriptor {
        self.descriptor
            .get_or_init(|| descriptor_of(&self.info, EditorPresence::Unknown))
    }

    /// The wiring read from the plugin now. Latency is read only while the
    /// plugin is active, which is when VST3 makes the figure valid
    /// (`getLatencySamples` is `[UI-thread & Setup Done]`, read after
    /// `setActive(1)`); a loaded plugin reports none.
    fn wiring(&self, main: &MainThread) -> LoadedPlugin {
        let latency_samples = if self.active.is_active() {
            Samples(self.latency_samples(main) as usize)
        } else {
            Samples(0)
        };
        LoadedPlugin {
            inputs: bus_channels(&self.info.input_bus_channels, self.info.num_inputs),
            outputs: bus_channels(&self.info.output_bus_channels, self.info.num_outputs),
            input_topology: self.input_bus_topologies(main).into_iter().collect(),
            output_topology: self.output_bus_topologies(main).into_iter().collect(),
            latency_samples,
            tail: self.tail_figure(main),
            features: self.features(main),
            probed: probed::VST3,
        }
    }

    fn parameters(&self, main: &MainThread) -> Vec<ParameterInfo> {
        self.parameter_list(main)
    }

    /// Normalized `0..=1`, VST3's own domain. `None` for an id the plugin does
    /// not list (`getParamNormalized` would answer 0 for it) and for a plugin
    /// with no edit controller.
    fn parameter(&self, main: &MainThread, id: ParamId) -> Option<Normalized> {
        self.parameter_info(main, id)?;
        self.plugin
            .parameter_normalized(main, id)
            .map(Normalized::clamped)
    }

    /// Applied now, as `setParamNormalized` on the edit controller. A plugin
    /// with no controller has nothing to write to.
    ///
    /// That call updates the controller only (`ivsteditcontroller.h`: "update
    /// the according GUI element(s) only"). A component that is separate from
    /// its controller learns a value from `IParameterChanges` in a process call,
    /// and a plugin that is not active is not processing. So a write to an
    /// inactive plugin waits in `carried`, which the next activation delivers in
    /// its first block and `save_state` stores beside the component's stream.
    /// While active, the Controller queues the write to the Processor instead.
    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        let written = self.write_normalized(main, id, value);
        // A write the last activation left undelivered is older than this one.
        if let Some(slot) = self.carried.iter_mut().find(|(waiting, _)| *waiting == id) {
            slot.1 = value;
        } else if written && !self.active.is_active() && self.parameter_info(main, id).is_some() {
            self.carried.push((id, value));
        }
    }

    /// The plugin's own display string for `value`: the value is a *parameter*
    /// of the call, not the plugin's current one, which is what an automation
    /// lane drawing labels along a curve needs.
    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        self.plugin.parameter_text(main, id, value.get())
    }

    /// The inverse. A caller must leave its field unchanged on `None` rather
    /// than substituting a fallback: a fabricated `0.0` would be written into
    /// the user's preset.
    ///
    /// The buffer is a NUL-terminated UTF-16 copy the plugin borrows for the
    /// call, bounded at 128 units to match the `String128` the display
    /// direction uses.
    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        self.plugin
            .parameter_from_text(main, id, text)
            .map(Normalized::clamped)
    }

    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        self.save_state_with(main, &[])
    }

    /// Restores the state. The writes a rejoin carried are older than it, and
    /// are dropped; the writes the blob itself carries wait for the next
    /// activation.
    fn load_state(&mut self, main: &MainThread, bytes: &[u8]) -> PluginResult<()> {
        let pending = self.restore_state(main, bytes)?;
        if !self.active.is_active() {
            self.carried = pending;
        }
        Ok(())
    }

    /// The plugin's programs, flattened out of its named program lists.
    ///
    /// VST3 is the one format whose presets are **grouped**: programs live in
    /// lists attached to units, so each `Preset` carries its list's name as
    /// `bank`. The id keeps both coordinates, since the list id is
    /// plugin-chosen rather than a position in `program_lists()`. A program the
    /// plugin declines to name is kept with an empty name rather than skipped:
    /// the index is half the identifier, so dropping one would renumber every
    /// program after it within that list.
    fn presets(&mut self, main: &MainThread) -> Vec<Preset> {
        self.program_lists(main)
            .into_iter()
            .flat_map(|list| {
                let this = &*self;
                (0..list.program_count)
                    .map(move |index| {
                        let name = this.program_name(main, list.id, index).unwrap_or_default();
                        Preset::in_bank(
                            PresetId::Program {
                                list_id: list.id,
                                index,
                            },
                            name,
                            list.name.clone(),
                        )
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool {
        match self.program_write(main, id) {
            Some((param, value)) => {
                // A program replaces the plugin's other parameters too.
                self.carried.clear();
                self.set_parameter(main, param, value);
                true
            }
            None => false,
        }
    }

    /// The program the plugin currently has selected, read back off the same
    /// parameter [`load_preset`](Control::load_preset) writes. `None` when no
    /// unit publishes a program list.
    fn current_preset(&mut self, main: &MainThread) -> Option<PresetId> {
        // One unit walk, not two: `units()` is a COM round trip per unit and
        // enumerates the program lists to resolve each unit's `program_list`.
        let (list_id, owning_unit) = self
            .units(main)
            .into_iter()
            .find_map(|u| u.program_list.map(|list| (list, u.id)))?;
        let (param_id, step_count) = self.program_change_parameter_of(main, owning_unit)?;
        if step_count <= 0 {
            return None;
        }
        let normalized = self.parameter(main, param_id)?.get();
        // Inverse of the write: `FromNormalized` is `value * stepCount`, then
        // rounded; the parameter is a discrete list, so a value between two
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

    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode) {
        // Encoded onto the `IAutomationState` bitmask here, at the VST3 edge,
        // by the crate's own SDK-backed conversion; a no-op if the plugin does
        // not implement the interface.
        self.apply_automation_state(main, mode);
    }

    /// Nothing is cached: parameters are read live from the controller.
    fn rescan_parameters(&mut self, _main: &MainThread, scope: RescanScope) {
        let _ = scope;
    }

    fn rebuild_midi_mapping(&mut self, main: &MainThread) {
        self.rebuild_midi_cc_mapping(main);
    }

    /// What the plugin has asked of the host since the last take, as the
    /// core's requests, appended to `out`: every `restartComponent` flag that
    /// has a request ([`RestartFlags::raise_into`](tutti_vst3_ffi::RestartFlags::raise_into) says which). A host that
    /// drives a [`Lifecycle`](tutti_plugin_types::Lifecycle) feeds these to it.
    ///
    /// Wait-free and allocation-free up to `out`'s capacity, so it can run
    /// between blocks of a realtime-priority thread. Queued edits, progress
    /// and unit news stay for
    /// [`poll_plugin_notifications`](Vst3Loaded::poll_plugin_notifications) or
    /// [`discard_notifications`](Vst3Loaded::discard_notifications).
    fn take_requests(&mut self, _main: &MainThread, out: &mut Vec<Request>) {
        out.extend(tutti_plugin_types::taken(self.plugin.handler().requests()));
    }

    /// `false`: VST3 reads the mode only at activation (`setupProcessing`'s
    /// `kOffline`), so a live change is a reconfigure through the Lifecycle,
    /// whose `Prepare::mode` the next activation carries.
    fn set_render_mode(&mut self, _main: &MainThread, _mode: RenderMode) -> bool {
        false
    }

    /// Nothing: VST3 reports the edits a plugin's own editor makes in the
    /// block's `ProcessOutput`.
    fn take_reported_edits(&mut self, _main: &MainThread, _out: &mut Vec<(ParamId, Normalized)>) {}

    /// `0`: VST3 reports the edits a plugin's own editor makes in the block's
    /// `ProcessOutput`, not through a queue that can drop them.
    fn dropped_reported_edits(&self) -> u64 {
        0
    }
}

impl EditorWindow for Vst3Loaded {
    /// Returns `true` if the plugin actually publishes an editor view.
    ///
    /// **Asks `createView`, not `controller.is_some()`.** Those are different
    /// questions: nearly every VST3 has an edit controller — that is where
    /// parameters live — while only some also publish a UI. A host that shows
    /// an "open editor" button from this answer needs the stronger question.
    ///
    /// The view is created and immediately released — the same
    /// `createView(kEditor)` the SDK's own `editorhost` uses to decide there is
    /// a UI (`editorhost.cpp:207`). That costs a plugin-side allocation per
    /// call, so callers needing it per-frame should cache it; the DAW asks
    /// once, at scan time.
    ///
    /// A plugin with no controller at all answers `false`.
    fn has_editor(&self, main: &MainThread) -> bool {
        self.plugin.has_editor(main)
    }

    /// Creates the plugin editor, attaches it to `parent` and returns its
    /// initial pixel size. Only one editor may be open at a time per instance:
    /// opening a second replaces the first.
    ///
    /// `scale` is `parent`'s backing-store scale. On a physical-pixel window
    /// system (Win32, X11) it is sent through
    /// `IPlugViewContentScaleSupport::setContentScaleFactor` before
    /// `attached`, for a view that implements it; on macOS the view reads it
    /// from its own `NSView`, and it is not sent (see [`ContentScale`]).
    ///
    /// # Errors
    ///
    /// [`PluginError::Editor`] if the plugin has no controller, refuses to
    /// create a view, the view rejects this platform's window type, or
    /// `IPlugView::attached` fails.
    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        self.embed_editor(main, parent, scale)
            .map_err(|error| match error {
                Vst3Error::PluginError { code, .. } => {
                    PluginError::Editor(tutti_plugin_types::EditorError::PluginError(format!(
                        "IPlugView::attached failed with code {code}"
                    )))
                }
                other => plugin_error(other),
            })
    }

    /// Closes the editor if open, calling `IPlugView::removed`. No-op otherwise.
    fn close_editor(&mut self, main: &MainThread) {
        self.plugin.close_editor(main);
    }

    /// Runs one iteration of the event loop this host lends the plugin
    /// (`Linux::IRunLoop`): fire any timers that came due and dispatch any
    /// plugin file descriptor that became readable.
    ///
    /// **The embedder must call this from its UI thread, every frame, for as
    /// long as any editor of this plugin is open.** X11 has no ambient run loop
    /// the way Cocoa and Win32 do, so `iplugview.h` makes the loop the *host's*
    /// job: "the host has to call the event handler when the file descriptor is
    /// marked readable", and a registered timer "will be called repeatedly until
    /// it is unregistered". Nothing else drives them. Skip this and the editor
    /// opens, paints once, and then freezes — no redraws, no animation, no
    /// response to input.
    ///
    /// Cheap and non-blocking: the `poll` uses a zero timeout, so calling it on
    /// a frame where nothing is ready costs one syscall and returns. That is why
    /// it is safe to call unconditionally from a render loop.
    ///
    /// # Why this is not folded into `poll_plugin_notifications`
    ///
    /// The two look alike but run on different clocks, and merging them would
    /// break one or the other.
    /// [`poll_plugin_notifications`](Vst3Loaded::poll_plugin_notifications) drains
    /// queues that this host filled; the work is already done and arriving late
    /// only delays a UI update. This call *is* the plugin's event loop — its
    /// cadence sets the editor's frame rate, and a host that polls
    /// notifications a few times a second (perfectly adequate for parameter
    /// echoes) would render such an editor unusable. Equally, a host with no
    /// editor open should not be forced to pump a loop with nothing in it.
    /// Keeping them separate lets each be called at the rate it actually needs.
    ///
    /// # Why the host and not a timer thread
    ///
    /// A background thread ticking this would be simpler for the embedder and
    /// is wrong: the handlers are plugin GUI code reaching into its X
    /// connection, and the spec's whole premise is that the host donates *its
    /// UI thread*. Calling them from anywhere else is the same data race as
    /// touching any other toolkit off-thread. So the obligation is the
    /// embedder's, and this method is the seam — it deliberately cannot be
    /// automated away from inside a library that owns no event loop.
    ///
    /// No-op on non-Linux targets, where the OS provides the run loop, so
    /// calling it unconditionally is portable.
    fn editor_idle(&mut self, main: &MainThread) {
        self.plugin.run_editor_loop_iteration(main);
    }

    /// The default capabilities: this surface reports no resize support.
    fn editor_capabilities(
        &mut self,
        _main: &tutti_plugin_types::MainThread,
    ) -> tutti_plugin_types::EditorCapabilities {
        tutti_plugin_types::EditorCapabilities::default()
    }

    /// Not supported here.
    fn set_editor_size(
        &mut self,
        _main: &tutti_plugin_types::MainThread,
        _requested: tutti_plugin_types::EditorSize,
    ) -> tutti_plugin_types::PluginResult<tutti_plugin_types::EditorSize> {
        Err(tutti_plugin_types::PluginError::Editor(
            tutti_plugin_types::EditorError::PluginError("set_editor_size not supported".into()),
        ))
    }

    /// Nothing asked: this surface never queues a resize request.
    fn poll_editor_resize_request(
        &mut self,
        _main: &tutti_plugin_types::MainThread,
    ) -> Option<tutti_plugin_types::EditorSize> {
        None
    }
}
