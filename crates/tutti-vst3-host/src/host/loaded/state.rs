//! Programs and units, the compatibility JSON, and the state container.

use std::collections::HashSet;
use tutti_plugin_types::MainThread;

use super::*;
use crate::host::controller::latest_per_parameter;

impl Vst3Loaded {
    /// The plugin's program lists — the named preset sets its units select
    /// from. Empty if the plugin doesn't implement `IUnitInfo`.
    ///
    /// Read before [`units`](Self::units), which needs the list ids to tell a
    /// unit's real program list from one it names but the plugin never
    /// publishes. Main/UI thread.
    pub fn program_lists(&self, main: &MainThread) -> Vec<Vst3ProgramListInfo> {
        self.plugin.program_lists(main)
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
    pub fn units(&self, main: &MainThread) -> Vec<Vst3UnitInfo> {
        self.plugin.units(main)
    }

    /// Name of one program in a list, or `None` if the plugin doesn't
    /// implement `IUnitInfo` or refuses the index.
    ///
    /// `list_id` is a [`Vst3ProgramListInfo::id`], **not** an index into
    /// [`program_lists`](Self::program_lists) — the two spaces differ, and a
    /// plugin is free to number its lists however it likes. Main/UI thread.
    pub fn program_name(
        &self,
        main: &MainThread,
        list_id: i32,
        program_index: u32,
    ) -> Option<String> {
        self.plugin.program_name(main, list_id, program_index)
    }

    /// The unit the plugin currently considers selected — which part of a
    /// multitimbral instrument its editor is showing. `None` if the plugin
    /// doesn't implement `IUnitInfo`.
    ///
    /// Unlike the other accessors here this cannot fail: `getSelectedUnit`
    /// returns the id directly with no result code, so a plugin that
    /// implements the interface always answers. Main/UI thread.
    pub fn selected_unit(&self, main: &MainThread) -> Option<crate::UnitId> {
        self.plugin.selected_unit(main)
    }

    /// Tells the plugin which unit the host's UI is now showing, so a plugin
    /// with its own editor can follow. Returns `false` if the plugin doesn't
    /// implement `IUnitInfo` or refuses the id.
    ///
    /// The one write on this interface. Pass
    /// [`UnitId::ROOT`] to select the implicit
    /// top-level unit. Main/UI thread.
    #[must_use]
    pub fn select_unit(&mut self, main: &MainThread, unit_id: crate::UnitId) -> bool {
        self.plugin.select_unit(main, unit_id)
    }

    /// Which unit a given bus channel belongs to, or `None` if the plugin
    /// doesn't implement `IUnitInfo` or maps nothing to that channel.
    ///
    /// Main/UI thread.
    pub fn unit_by_bus(
        &self,
        main: &MainThread,
        media_type: crate::MediaType,
        direction: crate::BusDirection,
        bus_index: i32,
        channel: i32,
    ) -> Option<crate::UnitId> {
        self.plugin
            .unit_by_bus(main, media_type, direction, bus_index, channel)
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
    pub fn compatibility_json(&self, _main: &MainThread) -> Option<String> {
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
    /// The component's stream holds only what a process call has delivered. A
    /// write made while the plugin is not active waits in `carried` for the
    /// next activation's first block, and one handed to a Processor waits for a
    /// block to take it (`undrained`, from the Controller), so the blob stores
    /// both beside the streams (container version 2) and `load_state` puts them
    /// back. A write a plugin makes itself (`performEdit`) while no block runs
    /// is the plugin's to carry; the host sees it only if the app relays it
    /// through `set_parameter`.
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
    pub(in crate::host) fn read_state(
        &self,
        main: &MainThread,
        undrained: &[(ParamId, Normalized)],
    ) -> Result<Vec<u8>> {
        let component = self.read_component_state(main)?;
        let controller = self.read_controller_state(main);
        let pending = latest_per_parameter(
            self.carried
                .iter()
                .chain(undrained)
                .copied()
                .collect::<Vec<_>>(),
        );
        Ok(pack_state_with_pending(&component, &controller, &pending))
    }

    /// Reads the controller's own `getState` stream, or `None` when there is no
    /// controller, it does not implement the call, or it fails.
    ///
    /// Failure is folded into `None` rather than surfaced: see
    /// [`save_state`](tutti_plugin_types::Control::save_state) for why a controller's UI state is best-effort
    /// while the component's is not.
    pub(super) fn read_controller_state(&self, main: &MainThread) -> Option<Vec<u8>> {
        self.plugin.controller_state(main)
    }

    /// Writes the component's `getState` blob into a fresh `IBStream` and return
    /// the bytes. Shared by [`save_state`](tutti_plugin_types::Control::save_state) and the load-time
    /// controller state-sync in [`initialize`](Self::initialize).
    ///
    /// A `kResultFalse` return (plugin has no state) yields an empty blob;
    /// only a genuine error tresult is surfaced as [`Vst3Error::PluginError`].
    pub(super) fn read_component_state(&self, main: &MainThread) -> Result<Vec<u8>> {
        self.plugin
            .component_state(main)
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
    pub(super) fn push_controller_state(&self, main: &MainThread, data: &[u8]) {
        self.plugin.sync_controller_to_component_state(main, data);
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
    /// Returns the version 2 container's pending writes, already applied to the
    /// controller: the caller owes the component those that its next block has
    /// not seen.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::StateError`] if the
    /// `IBStream` wrapper cannot be created, or
    /// [`Vst3Error::PluginError`] if the plugin
    /// rejects the component blob via `setState`. A controller that rejects its
    /// own stream is tolerated, matching [`save_state`](tutti_plugin_types::Control::save_state).
    pub(in crate::host) fn write_state(
        &mut self,
        main: &MainThread,
        data: &[u8],
    ) -> Result<Vec<(ParamId, Normalized)>> {
        if data.is_empty() {
            return Ok(Vec::new());
        }

        let Unpacked {
            component,
            controller,
            pending,
        } = unpack_full(data);

        // A plugin with no component state still gets the controller's stream
        // and the pending writes: the writes are for its separate controller's
        // parameters, which the component's stream cannot hold.
        if !component.is_empty() {
            self.plugin
                .set_component_state(main, component)
                .map_err(|e| Vst3Error::PluginError {
                    stage: LoadStage::Initialization,
                    code: e.code(),
                })?;

            self.push_controller_state(main, component);
        }

        if let Some(controller) = controller {
            self.push_controller_own_state(main, controller);
        }

        // The writes the component's stream did not yet hold are newer than it:
        // back to the controller. Bounded by the plugin's parameter list, not
        // by the blob: an id this plugin does not list is dropped, and each
        // parameter is kept once.
        let listed: HashSet<u32> = (0..self.parameter_count(main))
            .filter_map(|i| self.parameter_info_at(main, i))
            .map(|info| info.id())
            .collect();
        Ok(latest_listed(pending, &listed)
            .into_iter()
            .filter(|&(id, value)| self.write_normalized(main, id, value))
            .collect())
    }

    /// Hands the controller its own `setState` stream — the half
    /// `setComponentState` does not cover.
    ///
    /// Best-effort for the same reason as
    /// [`read_controller_state`](Self::read_controller_state): losing the UI's
    /// scroll position must not fail a project load whose audio state restored
    /// perfectly.
    pub(super) fn push_controller_own_state(&self, main: &MainThread, data: &[u8]) {
        self.plugin.set_controller_state(main, data);
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

/// Container version that also stores the parameter writes the component's
/// stream does not hold yet. Written only when there are some, so a blob with
/// nothing pending is version 1's, byte for byte. A build that predates it
/// reads the whole blob as a component stream.
pub(super) const STATE_VERSION_PENDING: u8 = 2;

/// Bytes of one pending write in a version 2 container: a `u32` id and an
/// `f64` value, little-endian.
const PENDING_BYTES: usize = 12;

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

/// [`pack_state`], with the parameter writes the component's stream cannot hold
/// yet (`Vst3Loaded::carried`): magic, version 2, their count as a `u32`, each
/// write, then the component's length, the component and the controller as in
/// version 1. With nothing pending the blob is [`pack_state`]'s.
pub(super) fn pack_state_with_pending(
    component: &[u8],
    controller: &Option<Vec<u8>>,
    pending: &[(ParamId, Normalized)],
) -> Vec<u8> {
    let (Ok(count), Ok(len), false) = (
        u32::try_from(pending.len()),
        u32::try_from(component.len()),
        pending.is_empty(),
    ) else {
        return pack_state(component, controller);
    };
    let controller = controller.as_deref().unwrap_or_default();
    let mut out = Vec::with_capacity(
        STATE_MAGIC.len() + 9 + pending.len() * PENDING_BYTES + component.len() + controller.len(),
    );
    out.extend_from_slice(STATE_MAGIC);
    out.push(STATE_VERSION_PENDING);
    out.extend_from_slice(&count.to_le_bytes());
    for (id, value) in pending {
        out.extend_from_slice(&id.get().to_le_bytes());
        out.extend_from_slice(&value.get().to_le_bytes());
    }
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(component);
    out.extend_from_slice(controller);
    out
}

/// `pending` with each listed parameter once, at its latest value, in the order
/// each was first written. One pass: a crafted blob of a million duplicates
/// costs a million steps, not a million times the parameter count.
fn latest_listed(
    pending: Vec<(ParamId, Normalized)>,
    listed: &HashSet<u32>,
) -> Vec<(ParamId, Normalized)> {
    let mut at: HashMap<ParamId, usize> = HashMap::new();
    let mut out: Vec<(ParamId, Normalized)> = Vec::new();
    for (id, value) in pending
        .into_iter()
        .filter(|(id, _)| listed.contains(&id.get()))
    {
        match at.entry(id) {
            std::collections::hash_map::Entry::Occupied(slot) => out[*slot.get()].1 = value,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(out.len());
                out.push((id, value));
            }
        }
    }
    out
}

/// What [`unpack_full`] found in a blob.
#[derive(Debug, PartialEq)]
pub(super) struct Unpacked<'a> {
    pub(super) component: &'a [u8],
    pub(super) controller: Option<&'a [u8]>,
    /// The version 2 container's writes, oldest first; empty otherwise.
    pub(super) pending: Vec<(ParamId, Normalized)>,
}

/// [`unpack_state`], and the pending writes of a version 2 container. A
/// version 2 blob that does not parse is a bare component stream, as a
/// malformed version 1 one is.
pub(super) fn unpack_full(data: &[u8]) -> Unpacked<'_> {
    let bare = |(component, controller)| Unpacked {
        component,
        controller,
        pending: Vec::new(),
    };
    let version_2 = data
        .strip_prefix(STATE_MAGIC.as_slice())
        .and_then(|rest| rest.strip_prefix(&[STATE_VERSION_PENDING]));
    let Some(rest) = version_2 else {
        return bare(unpack_state(data));
    };
    let parsed = (|| {
        let (count, rest) = rest.split_first_chunk::<4>()?;
        let table_len = (u32::from_le_bytes(*count) as usize).checked_mul(PENDING_BYTES)?;
        let (table, body) = rest.split_at_checked(table_len)?;
        let pending = table
            .as_chunks::<PENDING_BYTES>()
            .0
            .iter()
            .map(|entry| {
                let (id, value) = entry.split_first_chunk::<4>()?;
                Some((
                    ParamId::new(u32::from_le_bytes(*id)),
                    Normalized::clamped(f64::from_le_bytes(value.try_into().ok()?)),
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        let (component, controller) = split_body(body)?;
        Some(Unpacked {
            component,
            controller,
            pending,
        })
    })();
    parsed.unwrap_or_else(|| bare((data, None)))
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
    data.strip_prefix(STATE_MAGIC.as_slice())
        .and_then(|rest| rest.strip_prefix(&[STATE_VERSION]))
        .and_then(split_body)
        .unwrap_or((data, None))
}

/// The component length, the component and the controller that follow the
/// version byte (and, in version 2, the pending table). `None` when the length
/// field is short or points past the end.
fn split_body(body: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    let (len, payload) = body.split_first_chunk::<4>()?;
    let (component, controller) = payload.split_at_checked(u32::from_le_bytes(*len) as usize)?;
    Some((component, (!controller.is_empty()).then_some(controller)))
}

#[cfg(test)]
mod state_container_tests {
    use super::{
        Normalized, ParamId, STATE_MAGIC, STATE_VERSION, STATE_VERSION_PENDING, Unpacked,
        latest_listed, pack_state, pack_state_with_pending, unpack_full, unpack_state,
    };

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
        unknown_version.push(STATE_VERSION_PENDING.wrapping_add(1));
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

    fn writes() -> [(ParamId, Normalized); 2] {
        [
            (ParamId::new(7), Normalized::of(0.75)),
            (ParamId::new(1), Normalized::of(0.125)),
        ]
    }

    /// The pending writes come back in order with both halves intact.
    ///
    /// Mutation (run): writing the count as 0 in `pack_state_with_pending`
    /// fails this.
    #[test]
    fn pending_writes_round_trip_beside_both_halves() {
        let packed = pack_state_with_pending(b"component", &Some(b"ui".to_vec()), &writes());
        assert_eq!(
            unpack_full(&packed),
            Unpacked {
                component: b"component",
                controller: Some(b"ui".as_slice()),
                pending: writes().to_vec(),
            }
        );
        // A plugin with no controller stream still gets a header, because the
        // writes need one.
        let bare = pack_state_with_pending(b"component", &None, &writes());
        assert_eq!(unpack_full(&bare).pending, writes().to_vec());
        assert_eq!(unpack_full(&bare).controller, None);
    }

    /// With nothing pending the blob is version 1's, byte for byte, for a
    /// plugin with a controller stream and for one without.
    ///
    /// Mutation (run): choosing version 2 whenever the slice is empty fails
    /// both comparisons.
    #[test]
    fn nothing_pending_packs_to_the_version_1_container() {
        let ui = Some(b"ui".to_vec());
        assert_eq!(
            pack_state_with_pending(b"component", &ui, &[]),
            pack_state(b"component", &ui)
        );
        assert_eq!(
            pack_state_with_pending(b"component", &None, &[]),
            b"component"
        );
    }

    /// Version 1 and bare blobs read as having nothing pending, so every blob
    /// saved before version 2 still loads.
    #[test]
    fn older_blobs_read_with_nothing_pending() {
        let v1 = pack_state(b"component", &Some(b"ui".to_vec()));
        let got = unpack_full(&v1);
        assert_eq!(
            (got.component, got.controller, got.pending.len()),
            (b"component".as_slice(), Some(b"ui".as_slice()), 0)
        );
        let got = unpack_full(b"bare component stream");
        assert_eq!(got.component, b"bare component stream");
        assert!(got.pending.is_empty());
    }

    /// A version 2 container that does not parse is a bare component stream,
    /// like a malformed version 1: a length there is no reason to trust is not
    /// used to split the component.
    ///
    /// Mutation (run): taking the body whole when `split_body` refuses it fails
    /// this; so does a count of 0. Not covered: the `checked_mul` on the table
    /// length, which only matters where `usize` is 32 bits.
    #[test]
    fn a_malformed_version_2_container_falls_back_to_the_whole_blob() {
        let good = pack_state_with_pending(b"component", &Some(b"ui".to_vec()), &writes());
        let header = STATE_MAGIC.len() + 1;
        let table_end = header + 4 + 2 * 12;
        let mut cases = vec![
            // Cut inside the count, inside the table, and inside the length.
            good[..header + 2].to_vec(),
            good[..header + 4 + 5].to_vec(),
            good[..table_end + 2].to_vec(),
        ];
        // A count that claims more entries than the blob holds.
        let mut counted = good.clone();
        counted[header..header + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        cases.push(counted);
        // A component length past the end.
        let mut long = good.clone();
        long[table_end..table_end + 4].copy_from_slice(&99u32.to_le_bytes());
        cases.push(long);
        for case in cases {
            let got = unpack_full(&case);
            assert_eq!(got.component, case.as_slice(), "{case:?}");
            assert!(got.controller.is_none() && got.pending.is_empty());
        }
        assert_eq!(good[header - 1], STATE_VERSION_PENDING);
    }

    /// A million duplicates, and ids the plugin does not list, come out as one
    /// write per listed parameter at its latest value: what a first block has
    /// to prepend is bounded by the parameter list, whatever the blob holds.
    /// Counted, not timed.
    ///
    /// Mutation (run): returning the listed entries without merging duplicates
    /// fails the length.
    #[test]
    fn duplicate_and_unlisted_writes_load_as_one_per_listed_parameter() {
        let listed: std::collections::HashSet<u32> = [7, 1].into();
        let pending: Vec<_> = (0..1_000_000u32)
            .map(|i| {
                let id = [7, 1, 1000 + i][(i % 3) as usize];
                (
                    ParamId::new(id),
                    Normalized::clamped(f64::from(i % 100) / 100.0),
                )
            })
            .collect();
        let last_of = |id: u32| pending.iter().rev().find(|(p, _)| p.get() == id).unwrap().1;
        let (last_7, last_1) = (last_of(7), last_of(1));
        let got = latest_listed(pending, &listed);
        assert_eq!(got, [(ParamId::new(7), last_7), (ParamId::new(1), last_1)]);
    }
}
