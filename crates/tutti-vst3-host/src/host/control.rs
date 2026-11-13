//! The control surface of a VST3 plugin in the shared vocabulary, implemented
//! by [`Vst3Loaded`] and, with parameter writes redirected, by
//! [`Vst3Controller`](super::Vst3Controller).

use tutti_plugin_types::MainThread;
use tutti_plugin_types::features::probed;
use tutti_plugin_types::{
    AutomationMode, ContentScale, Control, EditorPresence, EditorSize, EditorWindow, Features,
    KeyEvent, KeyPhase, KeyUse, LoadedPlugin, ModifierConvention, Normalized, ParamEdit, ParamId,
    ParamMapping, ParameterInfo, PluginClass, PluginDescriptor, PluginError, PluginResult,
    PluginTail, Preset, PresetId, RenderModeSupport, Request, RescanScope, Samples, StateUse,
    Vst3SubCategories, WindowHandle,
};

use crate::error::Vst3Error;
use crate::types::PluginInfo;

use super::keys::view_key_of;
use super::loaded::Vst3Loaded;
use crate::BusDirection;

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
        measured: None,
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
        // `processMode` is a field of every plugin's `ProcessSetup`.
        features.insert(Features::RENDER_MODE);
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
            inputs: self.buses(main, BusDirection::Input),
            outputs: self.buses(main, BusDirection::Output),
            latency_samples,
            tail: self.tail_figure(main),
            features: self.features(main),
            probed: probed::VST3,
        }
    }

    fn parameters(&self, main: &MainThread) -> Vec<ParameterInfo> {
        self.parameter_list(main)
    }

    fn parameter_count(&self, main: &MainThread) -> usize {
        Vst3Loaded::parameter_count(self, main) as usize
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

    /// The controller's own `normalizedParamToPlain`, whatever shape it has
    /// ([`ControllerMapping`](crate::ControllerMapping)). `None` for an id the plugin does not list or with
    /// no controller.
    fn parameter_plain(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<f64> {
        Some(self.mapping(main, id)?.plain(value))
    }

    /// The controller's own `plainParamToNormalized`, clamped onto `0..=1` (the
    /// SDK does not bound it).
    fn parameter_normalized(
        &self,
        main: &MainThread,
        id: ParamId,
        plain: f64,
    ) -> Option<Normalized> {
        Some(self.mapping(main, id)?.normalized(plain))
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

    fn save_state(&mut self, main: &MainThread, _: StateUse) -> PluginResult<Vec<u8>> {
        self.save_state_with(main, &[])
    }

    /// Restores the state. The writes a rejoin carried are older than it, and
    /// are dropped; the writes the blob itself carries wait in `carried`, for the
    /// next activation, or (active) for the Controller to queue to the Processor.
    fn load_state(&mut self, main: &MainThread, bytes: &[u8], _: StateUse) -> PluginResult<()> {
        self.carried = self.restore_state(main, bytes)?;
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
                if self.active.is_active() {
                    // The Controller of this activation queues it to the Processor.
                    self.carried.push((param, value));
                }
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
        // rounded; the parameter is an unassigned list, so a value between two
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

    /// `OnActivation`: VST3 reads the mode only in `setupProcessing`
    /// (`kOffline`), a call on an inactive plugin.
    fn render_mode_support(&self, _main: &MainThread) -> RenderModeSupport {
        RenderModeSupport::OnActivation
    }

    /// The edits the plugin's own editor made, in order, as [`ParamEdit`]s:
    /// each `beginEdit`, `performEdit` (the normalized value it reported) and
    /// `endEdit` it called on the host's `IComponentHandler`
    /// (ivsteditcontroller.h, "UI-thread"). The same calls are how a plugin
    /// with a separate component and controller tells the host a value
    /// changed, so the host must hand them on to the processor; the activated
    /// [`Vst3Controller`](crate::Vst3Controller) does, and a plugin that is not
    /// active has no processor to hand them to.
    ///
    /// `beginEdit` and `endEdit` carry no value in VST3, and are reported
    /// as the gesture's ends. They are also kept for
    /// [`poll_plugin_notifications`](Vst3Loaded::poll_plugin_notifications),
    /// as every other notification popped on the way is (dirty state, progress,
    /// unit news). The processor's `outputParameterChanges` are not here: they
    /// are not an editor's, and the [`Vst3Controller`](crate::Vst3Controller)
    /// adds them on [`flush_edits`](Control::flush_edits).
    fn take_reported_edits(&mut self, _main: &MainThread, out: &mut Vec<ParamEdit>) {
        use crate::{ParameterEditEvent, PluginNotification};
        while let Some(notification) = self.plugin.handler().pop_notification() {
            match notification {
                PluginNotification::Edit(ParameterEditEvent::PerformEdit { param_id, value }) => {
                    out.push(ParamEdit::change(
                        param_id.into(),
                        Normalized::clamped(value),
                    ));
                }
                PluginNotification::Edit(ParameterEditEvent::BeginEdit(id)) => {
                    out.push(ParamEdit::Begin(id.into()));
                    self.stashed
                        .push(PluginNotification::Edit(ParameterEditEvent::BeginEdit(id)));
                }
                PluginNotification::Edit(ParameterEditEvent::EndEdit(id)) => {
                    out.push(ParamEdit::End(id.into()));
                    self.stashed
                        .push(PluginNotification::Edit(ParameterEditEvent::EndEdit(id)));
                }
                other => self.stashed.push(other),
            }
        }
    }

    /// The notifications the host's bounded queue refused because nobody
    /// drained it, edits among them ([`NOTIFICATION_CAPACITY`](crate::NOTIFICATION_CAPACITY)).
    fn dropped_reported_edits(&self) -> u64 {
        self.plugin.handler().dropped_notifications()
    }
}

impl EditorWindow for Vst3Loaded {
    /// Forwards the key as `onKeyDown` or `onKeyUp` and reports the view's `kResultTrue`
    /// as [`KeyUse::Used`]. A key VST3 cannot write (a character outside the Basic
    /// Multilingual Plane) is not sent and is [`KeyUse::Ignored`], as is any key while
    /// no editor is open.
    fn editor_key(&mut self, main: &MainThread, event: KeyEvent) -> PluginResult<KeyUse> {
        let Some(key) = view_key_of(&event, ModifierConvention::HOST) else {
            return Ok(KeyUse::Ignored);
        };
        let used = match event.phase {
            KeyPhase::Down => {
                self.plugin
                    .editor_key_down(main, key.character, key.virtual_key, key.modifiers)
            }
            KeyPhase::Up => {
                self.plugin
                    .editor_key_up(main, key.character, key.virtual_key, key.modifiers)
            }
        };
        Ok(if used { KeyUse::Used } else { KeyUse::Ignored })
    }

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

    /// Nothing: the event loop this host lends the plugin (`Linux::IRunLoop`) is
    /// not a tick. X11 has no ambient run loop the way Cocoa and Win32 do, so
    /// `iplugview.h` makes the loop the *host's* job: "the host has to call the
    /// event handler when the file descriptor is marked readable", and a
    /// registered timer "will be called repeatedly until it is unregistered".
    /// The host's main loop waits on the descriptors and the earliest timer
    /// together ([`wakeups`](EditorWindow::wakeups)) and hands back what fired
    /// ([`wake`](EditorWindow::wake)); the plugin server's loop blocks on them,
    /// and an in-process host's `editor_idle` looks without blocking
    /// (`tutti-plugin-host`'s `service_now`). A format that scanned
    /// them from this tick would run a loop of its own beside the host's.
    ///
    /// The handlers are plugin GUI code reaching into its X connection, and the
    /// spec's premise is that the host donates *its UI thread*: they run on the
    /// thread that holds the [`MainThread`], never a timer thread.
    fn editor_idle(&mut self, _main: &MainThread) {}

    /// The descriptors the plugin registered event handlers for and the
    /// earliest of its timers (Linux; nothing elsewhere, where the OS has the
    /// loop). The run loop is the module's, shared by its plugins.
    fn wakeups(&mut self, main: &MainThread, out: &mut tutti_plugin_types::Wakeups) {
        self.plugin.editor_wakeups(main, out);
    }

    /// Runs the handlers of the descriptors that were ready and the timers due
    /// at `now`, on the main thread.
    fn wake(
        &mut self,
        main: &MainThread,
        ready: &[tutti_plugin_types::Ready],
        now: std::time::Instant,
    ) {
        self.plugin.wake_editor_loop(main, ready, now);
    }

    /// What the open editor's view supports, queried live (the inherent
    /// [`Vst3Loaded::editor_capabilities`]).
    fn editor_capabilities(&mut self, main: &MainThread) -> tutti_plugin_types::EditorCapabilities {
        Vst3Loaded::editor_capabilities(self, main)
    }

    /// `IPlugViewContentScaleSupport` through [`Vst3Loaded::rescale_editor`]; a
    /// logical-pixel view or one without the interface is a
    /// [`ScaleUnsupported`](tutti_plugin_types::EditorError::ScaleUnsupported).
    fn set_editor_scale(
        &mut self,
        main: &MainThread,
        scale: tutti_plugin_types::ContentScale,
    ) -> tutti_plugin_types::PluginResult<()> {
        match Vst3Loaded::rescale_editor(self, main, scale) {
            Ok(true) => Ok(()),
            Ok(false) => Err(tutti_plugin_types::PluginError::Editor(
                tutti_plugin_types::EditorError::ScaleUnsupported,
            )),
            Err(e) => Err(tutti_plugin_types::PluginError::Editor(
                tutti_plugin_types::EditorError::PluginError(format!(
                    "VST3 rescale_editor failed: {e}"
                )),
            )),
        }
    }

    /// `IPlugView::onSize` through [`Vst3Loaded::resize_editor`]: the size the
    /// plugin snapped to.
    fn set_editor_size(
        &mut self,
        main: &MainThread,
        requested: tutti_plugin_types::EditorSize,
    ) -> tutti_plugin_types::PluginResult<tutti_plugin_types::EditorSize> {
        Vst3Loaded::resize_editor(self, main, requested).map_err(|e| {
            tutti_plugin_types::PluginError::Editor(tutti_plugin_types::EditorError::PluginError(
                format!("VST3 resize_editor failed: {e}"),
            ))
        })
    }

    /// The latest `IPlugFrame::resizeView` the plugin made since the last poll
    /// ([`Vst3Loaded::poll_editor_resize_request`]).
    fn poll_editor_resize_request(
        &mut self,
        main: &MainThread,
    ) -> Option<tutti_plugin_types::EditorSize> {
        Vst3Loaded::poll_editor_resize_request(self, main)
    }
}
