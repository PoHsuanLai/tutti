//! Programs and units, the compatibility JSON, and the state container.

use super::*;

impl Vst3Loaded {
    /// The plugin's program lists — the named preset sets its units select
    /// from. Empty if the plugin doesn't implement `IUnitInfo`.
    ///
    /// Read before [`units`](Self::units), which needs the list ids to tell a
    /// unit's real program list from one it names but the plugin never
    /// publishes. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn program_lists(&self) -> Vec<Vst3ProgramListInfo> {
        tutti_plugin_types::assert_main_thread();
        self.plugin.program_lists()
    }

    /// The plugin's unit tree — the groups it sorts parameters, programs and
    /// note expressions into. Empty if the plugin doesn't implement
    /// `IUnitInfo`.
    ///
    /// Returned flat, in the plugin's own order, with each unit naming its
    /// parent. Assembling a tree is the caller's job: a host that only wants to
    /// label parameter groups never needs one, and the shapes a plugin can
    /// report (orphans, cycles) have no single right resolution to bake in
    /// here.
    ///
    /// A unit whose `programListId` names no published list gets
    /// `program_list: None` — see [`Vst3UnitInfo::program_list`] for why, and
    /// [`Vst3UnitInfo::has_dangling_program_list`] to detect it. Main/UI
    /// thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn units(&self) -> Vec<Vst3UnitInfo> {
        tutti_plugin_types::assert_main_thread();
        self.plugin.units()
    }

    /// Name of one program in a list, or `None` if the plugin doesn't
    /// implement `IUnitInfo` or refuses the index.
    ///
    /// `list_id` is a [`Vst3ProgramListInfo::id`], **not** an index into
    /// [`program_lists`](Self::program_lists) — the two spaces differ, and a
    /// plugin is free to number its lists however it likes. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn program_name(&self, list_id: i32, program_index: u32) -> Option<String> {
        tutti_plugin_types::assert_main_thread();
        self.plugin.program_name(list_id, program_index)
    }

    /// The unit the plugin currently considers selected — which part of a
    /// multitimbral instrument its editor is showing. `None` if the plugin
    /// doesn't implement `IUnitInfo`.
    ///
    /// Unlike the other accessors here this cannot fail: `getSelectedUnit`
    /// returns the id directly with no result code, so a plugin that
    /// implements the interface always answers. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn selected_unit(&self) -> Option<i32> {
        tutti_plugin_types::assert_main_thread();
        self.plugin.selected_unit()
    }

    /// Tells the plugin which unit the host's UI is now showing, so a plugin
    /// with its own editor can follow. Returns `false` if the plugin doesn't
    /// implement `IUnitInfo` or refuses the id.
    ///
    /// The one write on this interface. Pass
    /// [`unit_ids::ROOT`] to select the implicit
    /// top-level unit. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    #[must_use]
    pub fn select_unit(&mut self, unit_id: i32) -> bool {
        tutti_plugin_types::assert_main_thread();
        self.plugin.select_unit(unit_id)
    }

    /// Which unit a given bus channel belongs to, or `None` if the plugin
    /// doesn't implement `IUnitInfo` or maps nothing to that channel.
    ///
    /// `media_type` and `direction` are the SDK's `MediaTypes` and
    /// `BusDirections` values. Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn unit_by_bus(
        &self,
        media_type: i32,
        direction: i32,
        bus_index: i32,
        channel: i32,
    ) -> Option<i32> {
        tutti_plugin_types::assert_main_thread();
        self.plugin
            .unit_by_bus(media_type, direction, bus_index, channel)
    }

    /// Reads the bundle's machine-readable compatibility / migration info as a
    /// JSON string, via the factory's `IPluginCompatibility` class (the
    /// moduleinfo "compatibility" section). Describes which older plugins this
    /// one can replace, so a host can offer to swap an unavailable plugin for a
    /// compatible successor and migrate its state.
    ///
    /// Unlike the other accessors this is a **factory-level** class, not a
    /// controller/processor extension: it enumerates the factory's classes,
    /// finds the one in the "Plugin Compatibility Class" category, instantiates
    /// it, and reads its JSON. Returns `None` if the bundle ships no such class.
    /// Main/UI thread.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub fn compatibility_json(&self) -> Option<String> {
        tutti_plugin_types::assert_main_thread();
        self._library.module().compatibility_json()
    }

    /// Captures the plugin's state as an opaque byte blob suitable for
    /// persisting and later feeding back to [`load_state`](tutti_plugin_types::Control::load_state).
    ///
    /// This carries **both** of the plugin's streams. The spec gives the
    /// controller its own `getState`/`setState` pair, distinct from the
    /// component's, holding what only the UI knows: scroll position, the
    /// selected tab, a meter's display mode. Saving just the component stream
    /// discards all of it on every save/restore.
    ///
    /// The two are length-prefixed rather than concatenated, because each half
    /// is a private format of unknown length — only an explicit length prefix
    /// can split them again.
    ///
    /// The bytes within each half are the plugin's private format; the host
    /// never interprets them. A plugin with no state at all yields an empty
    /// blob, which `set_state` accepts back as a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::StateError`] if the
    /// host-side `IBStream` wrapper cannot be created, or
    /// [`Vst3Error::PluginError`] if the plugin
    /// fails `getState`. A parameter dump is **not** substituted: a plugin's
    /// state covers more than parameters (active preset, internal DSP state,
    /// sample references), so a lossy synthetic blob would restore incorrectly
    /// while masquerading as faithful state.
    ///
    /// A controller that fails its own `getState` is **not** an error: the
    /// component half is the one a project cannot be restored without, and
    /// plenty of controllers have no UI state to give. That half degrades to
    /// empty and the component half is still returned.
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub(in crate::host) fn read_state(&self) -> Result<Vec<u8>> {
        tutti_plugin_types::assert_main_thread();
        let component = self.read_component_state()?;
        let controller = self.read_controller_state();
        Ok(pack_state(&component, &controller))
    }

    /// Reads the controller's own `getState` stream, or `None` when there is no
    /// controller, it does not implement the call, or it fails.
    ///
    /// Failure is folded into `None` rather than surfaced: see
    /// [`save_state`](tutti_plugin_types::Control::save_state) for why a controller's UI state is best-effort
    /// while the component's is not.
    pub(super) fn read_controller_state(&self) -> Option<Vec<u8>> {
        self.plugin.controller_state()
    }

    /// Writes the component's `getState` blob into a fresh `IBStream` and return
    /// the bytes. Shared by [`save_state`](tutti_plugin_types::Control::save_state) and the load-time
    /// controller state-sync in [`initialize`](Self::initialize).
    ///
    /// A `kResultFalse` return (plugin has no state) yields an empty blob;
    /// only a genuine error tresult is surfaced as [`Vst3Error::PluginError`].
    pub(super) fn read_component_state(&self) -> Result<Vec<u8>> {
        self.plugin
            .component_state()
            .map_err(|e| Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: e.code(),
            })
    }

    /// Feed a component-state blob to the edit controller via
    /// `IEditController::setComponentState`, so a separate controller reflects
    /// the processor's state. No-op if the plugin has no controller or the
    /// blob is empty. Errors from the controller are tolerated (some plugins
    /// return `kResultFalse` when they have no controller state to load) —
    /// this is best-effort synchronisation, shared by
    /// [`load_state`](tutti_plugin_types::Control::load_state) and [`initialize`](Self::initialize).
    pub(super) fn push_controller_state(&self, data: &[u8]) {
        self.plugin.sync_controller_to_component_state(data);
    }

    /// Restores plugin state from a blob produced by [`save_state`](tutti_plugin_types::Control::save_state).
    ///
    /// Three things happen, in the order the spec requires. The component gets
    /// its own stream via `IComponent::setState`. The controller is then shown
    /// that same component stream through `setComponentState`, so a separate
    /// controller mirrors the processor's values. Finally the controller gets
    /// its *own* stream via `IEditController::setState` — the UI state that
    /// `setComponentState` cannot carry.
    ///
    /// An empty blob (from a plugin that had no state to save) is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::StateError`] if the
    /// `IBStream` wrapper cannot be created, or
    /// [`Vst3Error::PluginError`] if the plugin
    /// rejects the component blob via `setState`. A controller that rejects its
    /// own stream is tolerated, matching [`save_state`](tutti_plugin_types::Control::save_state).
    ///
    /// # Panics
    ///
    /// In debug builds, panics if called off the thread registered with
    /// [`tutti_plugin_types::mark_main_thread`].
    pub(in crate::host) fn write_state(&mut self, data: &[u8]) -> Result<()> {
        tutti_plugin_types::assert_main_thread();
        if data.is_empty() {
            return Ok(());
        }

        let (component, controller) = unpack_state(data);
        if component.is_empty() {
            return Ok(());
        }

        self.plugin
            .set_component_state(component)
            .map_err(|e| Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: e.code(),
            })?;

        self.push_controller_state(component);

        if let Some(controller) = controller {
            self.push_controller_own_state(controller);
        }

        Ok(())
    }

    /// Hands the controller its own `setState` stream — the half
    /// `setComponentState` does not cover.
    ///
    /// Best-effort for the same reason as
    /// [`read_controller_state`](Self::read_controller_state): losing the UI's
    /// scroll position must not fail a project load whose audio state restored
    /// perfectly.
    pub(super) fn push_controller_own_state(&self, data: &[u8]) {
        self.plugin.set_controller_state(data);
    }
}

/// Magic prefixing a two-stream state blob. Chosen to be something no VST3
/// plugin would plausibly open its own private state with, so
/// [`unpack_state`] can tell a packed blob from a bare component stream saved
/// before this host carried the controller's half.
pub(super) const STATE_MAGIC: &[u8; 8] = b"TUTTIVS3";

/// Container version. Bump only if the layout after the magic changes; a
/// reader that meets a version it does not know falls back to treating the
/// whole blob as a bare component stream, which is wrong but recoverable —
/// where mis-splitting it would hand the plugin garbage.
pub(super) const STATE_VERSION: u8 = 1;

/// Pack the component and controller streams into one opaque blob.
///
/// Layout: magic, version, then the component length as a little-endian `u32`,
/// then the two payloads. The controller half is whatever remains, so it needs
/// no length of its own.
///
/// A plugin with no controller state at all packs to `component` alone, with
/// no header. That keeps the common case byte-identical to what this host
/// saved before the controller stream existed, so nothing re-saves a project
/// merely for having been opened.
pub(super) fn pack_state(component: &[u8], controller: &Option<Vec<u8>>) -> Vec<u8> {
    let Some(controller) = controller.as_ref().filter(|c| !c.is_empty()) else {
        return component.to_vec();
    };

    // A component stream too long to describe in a u32 cannot be split back
    // apart, so keep the half a project cannot be restored without and drop
    // the UI half. 4 GiB of component state is not a case worth a wider field.
    let Ok(len) = u32::try_from(component.len()) else {
        return component.to_vec();
    };

    let mut out = Vec::with_capacity(STATE_MAGIC.len() + 5 + component.len() + controller.len());
    out.extend_from_slice(STATE_MAGIC);
    out.push(STATE_VERSION);
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(component);
    out.extend_from_slice(controller);
    out
}

/// Splits a blob back into its component and controller halves.
///
/// Anything not carrying [`STATE_MAGIC`] is a bare component stream — either
/// saved by an older build of this host, or packed by [`pack_state`] for a
/// plugin with no controller state. Both restore correctly by handing the
/// whole blob to the component, which is what this host has always done.
///
/// A blob that starts with the magic but is then malformed (unknown version,
/// truncated, a length past the end) is treated the same way. That cannot
/// restore the controller half, but the alternative — splitting at a length
/// there is reason to distrust — hands the *component* a corrupt stream, and the
/// component half is the one a project cannot be restored without.
pub(super) fn unpack_state(data: &[u8]) -> (&[u8], Option<&[u8]>) {
    let Some(rest) = data.strip_prefix(STATE_MAGIC.as_slice()) else {
        return (data, None);
    };
    let Some((&version, rest)) = rest.split_first() else {
        return (data, None);
    };
    if version != STATE_VERSION {
        return (data, None);
    }
    if rest.len() < 4 {
        return (data, None);
    }
    let (len_bytes, payload) = rest.split_at(4);
    let len =
        u32::from_le_bytes(len_bytes.try_into().expect("split_at(4) yields 4 bytes")) as usize;
    if len > payload.len() {
        return (data, None);
    }

    let (component, controller) = payload.split_at(len);
    (component, (!controller.is_empty()).then_some(controller))
}

#[cfg(test)]
mod state_container_tests {
    use super::{STATE_MAGIC, STATE_VERSION, pack_state, unpack_state};

    /// Both halves survive the round trip, split back at the right byte.
    #[test]
    fn both_halves_survive_a_round_trip() {
        let component = b"component-private-bytes".as_slice();
        let controller = b"scroll=42;tab=2".to_vec();

        let packed = pack_state(component, &Some(controller.clone()));
        let (got_component, got_controller) = unpack_state(&packed);

        assert_eq!(got_component, component);
        assert_eq!(got_controller, Some(controller.as_slice()));
    }

    /// A blob saved before this host carried the controller's stream is a bare
    /// component blob with no header. It must still restore, or every existing
    /// project loses its plugin state.
    ///
    /// The fixture is binary rather than text, and its leading bytes are chosen
    /// to parse as a *valid* header if the magic check were skipped: a version
    /// byte, then a little-endian length well inside the blob. A plugin's
    /// private state is binary, so opening on such bytes is ordinary rather
    /// than contrived — and a reader that trusted them would hand the component
    /// the first three bytes of a twenty-byte stream and call the rest UI state.
    #[test]
    fn a_bare_component_blob_still_restores() {
        let mut legacy = vec![STATE_VERSION];
        legacy.extend_from_slice(&3u32.to_le_bytes());
        legacy.extend_from_slice(b"abcdefghijklmno");

        let (component, controller) = unpack_state(&legacy);
        assert_eq!(component, legacy.as_slice());
        assert_eq!(controller, None);
    }

    /// A plugin with no controller state packs to exactly the component bytes,
    /// so a project does not churn on disk merely for being opened by a build
    /// that knows about the second stream.
    #[test]
    fn no_controller_state_packs_to_the_bare_component_bytes() {
        let component = b"component-only".as_slice();
        assert_eq!(pack_state(component, &None), component);
        assert_eq!(pack_state(component, &Some(Vec::new())), component);
    }

    /// A component half whose own bytes begin with the magic must not be
    /// mistaken for a container when it is handed back unpacked.
    #[test]
    fn a_component_blob_starting_with_the_magic_round_trips() {
        let mut component = STATE_MAGIC.to_vec();
        component.extend_from_slice(b"...plugin's own bytes");
        let controller = b"ui".to_vec();

        let packed = pack_state(&component, &Some(controller.clone()));
        let (got_component, got_controller) = unpack_state(&packed);

        assert_eq!(got_component, component);
        assert_eq!(got_controller, Some(controller.as_slice()));
    }

    /// A truncated or otherwise malformed container degrades to "all of it is
    /// component state" rather than splitting at a length it cannot trust.
    /// Restoring the component half wrongly is worse than losing the UI half.
    #[test]
    fn a_malformed_container_falls_back_to_the_whole_blob() {
        let mut truncated = STATE_MAGIC.to_vec();
        truncated.push(STATE_VERSION);
        truncated.extend_from_slice(&[0u8, 1]); // a 2-byte length field, not 4
        assert_eq!(unpack_state(&truncated), (truncated.as_slice(), None));

        let mut past_the_end = STATE_MAGIC.to_vec();
        past_the_end.push(STATE_VERSION);
        past_the_end.extend_from_slice(&99u32.to_le_bytes());
        past_the_end.extend_from_slice(b"short");
        assert_eq!(unpack_state(&past_the_end), (past_the_end.as_slice(), None));

        let mut unknown_version = STATE_MAGIC.to_vec();
        unknown_version.push(STATE_VERSION.wrapping_add(1));
        unknown_version.extend_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            unpack_state(&unknown_version),
            (unknown_version.as_slice(), None)
        );
    }

    /// An empty controller half is spelled `None`, never `Some(&[])`, so a
    /// caller cannot hand a plugin a zero-length `setState` stream that means
    /// nothing.
    #[test]
    fn an_empty_controller_half_reads_back_as_absent() {
        let packed = pack_state(b"component", &Some(b"x".to_vec()));
        let (_, controller) = unpack_state(&packed);
        assert_eq!(controller, Some(b"x".as_slice()));

        // Built by hand: a container claiming the whole payload is component.
        let mut zero_tail = STATE_MAGIC.to_vec();
        zero_tail.push(STATE_VERSION);
        zero_tail.extend_from_slice(&9u32.to_le_bytes());
        zero_tail.extend_from_slice(b"component");
        assert_eq!(unpack_state(&zero_tail), (b"component".as_slice(), None));
    }
}
