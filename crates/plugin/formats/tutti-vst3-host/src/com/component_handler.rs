//! IComponentHandler (v1/v2/v3) + IComponentHandlerBusActivation + IProgress +
//! IUnitHandler (v1/v2) — multi-vtable host-side dispatcher for every plugin
//! callback.
//!
//! # Where the callbacks go, and why nothing here allocates
//!
//! Plugins call `performEdit`, `beginEdit`, `endEdit` and `restartComponent`
//! from their *process* thread. The spec says not to; many do anyway. So the
//! receiving half must be safe to run on an audio thread, which an unbounded
//! channel is not: its `send` allocates a block whenever the current one fills.
//!
//! - **Restart requests** are cumulative — the host only ever acts on the union
//!   of the flags since its last look — so they live in one `AtomicI32`,
//!   `fetch_or`'d by the plugin and swapped to zero by
//!   [`ComponentHandler::take_restart_bits`]. No queue, no capacity, nothing to
//!   lose.
//! - **Everything else** (edits, progress, unit news) goes on one bounded
//!   [`ArrayQueue`] of [`PluginNotification`], preallocated at construction.
//!   A push past [`NOTIFICATION_CAPACITY`] is dropped and counted
//!   ([`ComponentHandler::dropped_notifications`]), never logged and never
//!   grown, and it raises a sticky flag
//!   ([`ComponentHandler::take_overflowed`]) so the host knows to re-read.
//!
//! A restart therefore carries no position relative to the queued events. No
//! consumer relied on one: the only reader always folded every restart into a
//! single coalesced outcome, whatever edits surrounded it.
//!
//! `IProgress::start`'s description is inline too ([`ProgressDescription`],
//! truncated at [`PROGRESS_DESCRIPTION_CAPACITY`] bytes), so no callback
//! allocates, and dropping a queued notification frees nothing.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};

use arrayvec::ArrayString;
use crossbeam_queue::ArrayQueue;
use vst3::Steinberg::{
    kResultOk, tresult, FIDString, IPlugView, TBool,
    Vst::{
        BusDirection, IComponentHandler, IComponentHandler2, IComponentHandler2Trait,
        IComponentHandler3, IComponentHandler3Trait, IComponentHandlerBusActivation,
        IComponentHandlerBusActivationTrait, IComponentHandlerTrait, IContextMenu, IProgress,
        IProgressTrait,
        IProgress_::{ProgressType, ID},
        IUnitHandler, IUnitHandler2, IUnitHandler2Trait, IUnitHandlerTrait, MediaType, ParamID,
        ParamValue, ProgramListID, UnitID,
    },
};
use vst3::{Class, ComWrapper};

/// Long-running progress notifications emitted by plugins (sample loading,
/// offline rendering, etc.). Delivered via
/// [`Vst3Loaded::poll_plugin_notifications`](crate::Vst3Loaded::poll_plugin_notifications).
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// A new progress operation has begun. `id` uniquely identifies this
    /// operation across its lifetime; `progress_type` is the raw VST3
    /// `ProgressType` value; `description` is a human-readable label.
    Started {
        /// Identifies this operation across its lifetime; matches the `id` of
        /// the later `Updated` and `Finished` events.
        id: u64,
        /// The raw VST3 `ProgressType` value.
        progress_type: u32,
        /// Human-readable label supplied by the plugin, truncated to
        /// [`PROGRESS_DESCRIPTION_CAPACITY`] bytes.
        description: ProgressDescription,
    },
    /// Progress update, normalized to `0.0..=1.0`.
    Updated {
        /// The operation this update belongs to.
        id: u64,
        /// Completion fraction, `0.0..=1.0`.
        progress: f64,
    },
    /// The operation with this id has finished.
    Finished {
        /// The operation that ended. No further events carry this id.
        id: u64,
    },
}

/// How many UTF-8 bytes of an `IProgress::start` description are kept.
/// Longer descriptions are cut at the last whole character that fits.
pub const PROGRESS_DESCRIPTION_CAPACITY: usize = 128;

/// The label a plugin gave an `IProgress` operation, held inline so that
/// `IProgress::start` allocates nothing on the plugin's thread and dropping
/// the event frees nothing on the host's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgressDescription(ArrayString<PROGRESS_DESCRIPTION_CAPACITY>);

impl ProgressDescription {
    /// The description as text.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Decodes a NUL-terminated UTF-16 string, replacing unpaired surrogates
    /// with U+FFFD and stopping at the first character that does not fit.
    /// Reads no further than the terminator or the first character past the
    /// capacity.
    ///
    /// # Safety
    ///
    /// `ptr` is null or points at a NUL-terminated UTF-16 string.
    unsafe fn from_utf16_nul(ptr: *const u16) -> Self {
        let mut out = ArrayString::new();
        if ptr.is_null() {
            return Self(out);
        }
        let mut cursor = ptr;
        let units = std::iter::from_fn(|| {
            // SAFETY: the caller guarantees a terminator; reading stops at it.
            let unit = unsafe { *cursor };
            (unit != 0).then(|| {
                cursor = unsafe { cursor.add(1) };
                unit
            })
        });
        for c in char::decode_utf16(units) {
            if out
                .try_push(c.unwrap_or(char::REPLACEMENT_CHARACTER))
                .is_err()
            {
                break;
            }
        }
        Self(out)
    }
}

impl std::ops::Deref for ProgressDescription {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Display for ProgressDescription {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Unit / program-list change notifications from the plugin. Delivered via
/// [`Vst3Loaded::poll_plugin_notifications`](crate::Vst3Loaded::poll_plugin_notifications).
#[derive(Debug, Clone)]
pub enum UnitEvent {
    /// Plugin has selected a different unit (preset category / voice).
    UnitSelected(i32),
    /// Program *information* in a list went stale — a rename, a preset load, or
    /// a PitchName change (`ivstunits.h:88-92`). Not a selection change: the
    /// plugin is saying what it holds is no longer what the host cached, so the
    /// response is to re-read the list, not to move a cursor.
    ///
    /// `program_index` is `-1` (`kAllProgramInvalid`) when *every* program in
    /// the list is invalid, and only otherwise names a single one. That is a
    /// sentinel, not an index — spending it as one reads before the start of
    /// whatever array holds the list.
    ProgramListChanged {
        /// The program list to re-read.
        list_id: i32,
        /// A single stale program, or `-1` for "all of them" — see above.
        program_index: i32,
    },
    /// The unit ↔ bus mapping has changed (IUnitHandler2).
    UnitByBusChanged,
}

use vst3::Steinberg::Vst::RestartFlags_;

/// Decoded view of the `RestartFlags` bitmask the plugin passes to
/// `IComponentHandler::restartComponent`.
///
/// VST3 packs every "the host must re-sync X" signal into one `i32`. This
/// splits it into named booleans so consumers can branch without re-deriving
/// the bit math, and so the decode is unit-testable in isolation.
///
/// Spec reference: `Steinberg::Vst::RestartFlags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestartFlags {
    /// `kReloadComponent` — the plugin must be fully reloaded (deactivate +
    /// reactivate). The heaviest signal; usually emitted after an in-plugin
    /// preset load that changes the component structure.
    pub reload_component: bool,
    /// `kIoChanged` — input/output bus configuration changed; re-run bus
    /// enumeration and (eventually) arrangement negotiation.
    pub io_changed: bool,
    /// `kParamValuesChanged` — one or more parameter *values* changed (e.g. an
    /// in-plugin preset load); re-read parameter values into host state.
    pub param_values_changed: bool,
    /// `kLatencyChanged` — processing latency changed; re-read
    /// `getLatencySamples` and surface to PDC.
    pub latency_changed: bool,
    /// `kParamTitlesChanged` — parameter titles/units/flags changed; re-read
    /// parameter info.
    pub param_titles_changed: bool,
    /// `kMidiCCAssignmentChanged` — the `IMidiMapping` CC→param table changed;
    /// re-query it.
    pub midi_cc_assignment_changed: bool,
    /// `kNoteExpressionChanged` — note-expression support changed.
    pub note_expression_changed: bool,
    /// `kIoTitlesChanged` — bus titles changed (cosmetic).
    pub io_titles_changed: bool,
    /// `kPrefetchableSupportChanged` — offline prefetch support changed.
    pub prefetchable_support_changed: bool,
    /// `kRoutingInfoChanged` — channel routing info changed.
    pub routing_info_changed: bool,
    /// `kKeyswitchChanged` — keyswitch info changed.
    pub keyswitch_changed: bool,
    /// `kParamIDMappingChanged` — param-ID mapping changed.
    pub param_id_mapping_changed: bool,
}

impl RestartFlags {
    /// Decodes the raw `i32` bitmask passed to `restartComponent`.
    pub fn from_bits(flags: i32) -> Self {
        let has = |bit: i32| (flags & bit) != 0;
        Self {
            reload_component: has(RestartFlags_::kReloadComponent),
            io_changed: has(RestartFlags_::kIoChanged),
            param_values_changed: has(RestartFlags_::kParamValuesChanged),
            latency_changed: has(RestartFlags_::kLatencyChanged),
            param_titles_changed: has(RestartFlags_::kParamTitlesChanged),
            midi_cc_assignment_changed: has(RestartFlags_::kMidiCCAssignmentChanged),
            note_expression_changed: has(RestartFlags_::kNoteExpressionChanged),
            io_titles_changed: has(RestartFlags_::kIoTitlesChanged),
            prefetchable_support_changed: has(RestartFlags_::kPrefetchableSupportChanged),
            routing_info_changed: has(RestartFlags_::kRoutingInfoChanged),
            keyswitch_changed: has(RestartFlags_::kKeyswitchChanged),
            param_id_mapping_changed: has(RestartFlags_::kParamIDMappingChanged),
        }
    }
}

/// Notifications the plugin's editor pushes through `IComponentHandler` and
/// its v2/v3/bus-activation extensions, delivered via
/// [`Vst3Loaded::poll_plugin_notifications`](crate::Vst3Loaded::poll_plugin_notifications).
///
/// `restartComponent` is not among them: restart requests are cumulative, so
/// they are coalesced into the [`RestartOutcome`](crate::RestartOutcome) that
/// poll returns rather than queued one per call.
///
/// Typical usage: a user drags a knob in the plugin UI → the plugin calls
/// `beginEdit` / `performEdit` / `endEdit` → the host receives the matching
/// [`ParameterEditEvent`]s and updates its own automation state.
#[derive(Debug, Clone)]
pub enum ParameterEditEvent {
    /// Plugin is about to start editing the parameter (mouse-down on a knob).
    BeginEdit(u32),
    /// Plugin is reporting a new normalized (0.0 – 1.0) value for the
    /// parameter.
    PerformEdit {
        /// The parameter being edited.
        param_id: u32,
        /// Normalized value, `0.0..=1.0`.
        value: f64,
    },
    /// Plugin has finished editing the parameter (mouse-up).
    EndEdit(u32),
    /// Plugin has marked itself dirty — the host should treat project state
    /// as modified.
    SetDirty(bool),
    /// Plugin requests the host open its editor (e.g. from a context menu).
    RequestOpenEditor,
    /// Plugin signals the start of a coalescable group of edits.
    StartGroupEdit,
    /// Plugin signals the end of a coalescable group of edits.
    FinishGroupEdit,
    /// Plugin requests that a bus be activated or deactivated.
    RequestBusActivation {
        /// VST3 `MediaTypes` value: audio or event.
        media_type: i32,
        /// VST3 `BusDirections` value: `kInput` or `kOutput`.
        direction: i32,
        /// Bus index within that media type and direction.
        index: i32,
        /// `true` to activate the bus, `false` to deactivate it.
        state: bool,
    },
}

/// One queued plugin callback, as
/// [`Vst3Loaded::poll_plugin_notifications`](crate::Vst3Loaded::poll_plugin_notifications)
/// hands it over, in arrival order across all three kinds.
#[derive(Debug, Clone)]
pub enum PluginNotification {
    /// An `IComponentHandler`(2/bus-activation) call: an edit gesture, dirty
    /// state, an editor request.
    Edit(ParameterEditEvent),
    /// An `IProgress` report.
    Progress(ProgressEvent),
    /// An `IUnitHandler`(2) report.
    Unit(UnitEvent),
}

/// How many [`PluginNotification`]s the handler holds between two polls.
///
/// A UI that polls once a frame drains a few dozen at most, even mid-gesture.
/// The bound exists for the poller that stalls: the queue then stays at this
/// size instead of growing memory.
///
/// **Overflow drops the newest notification**, and counts it. Keeping the
/// oldest means a gesture can lose its last `PerformEdit` and its `EndEdit`,
/// so the host's view of a parameter may be stale. Each drop also raises a
/// sticky flag, reported once by the next poll as
/// [`RestartOutcome::notifications_overflowed`](crate::RestartOutcome::notifications_overflowed):
/// a host seeing it re-reads every parameter value, as for
/// `kParamValuesChanged`. (Dropping the oldest instead would lose the
/// `BeginEdit` and break a gesture just the same; there is no order-safe drop.)
pub const NOTIFICATION_CAPACITY: usize = 512;

/// The host's `IComponentHandler` (and friends). See the module docs for where
/// each callback lands.
pub struct ComponentHandler {
    /// Edits, progress and unit news, in arrival order.
    notifications: ArrayQueue<PluginNotification>,
    /// Pushes refused because `notifications` was full.
    dropped: AtomicU64,
    /// Set by every refused push, cleared by `take_overflowed`.
    overflowed: AtomicBool,
    /// The union of every `restartComponent` flag word since the last take.
    restart_bits: AtomicI32,
    next_progress_id: AtomicU64,
}

impl Class for ComponentHandler {
    type Interfaces = (
        IComponentHandler,
        IComponentHandler2,
        IComponentHandler3,
        IComponentHandlerBusActivation,
        IProgress,
        IUnitHandler,
        IUnitHandler2,
    );
}

impl ComponentHandler {
    /// A handler with an empty, preallocated notification queue.
    pub fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            notifications: ArrayQueue::new(NOTIFICATION_CAPACITY),
            dropped: AtomicU64::new(0),
            overflowed: AtomicBool::new(false),
            restart_bits: AtomicI32::new(0),
            next_progress_id: AtomicU64::new(1),
        })
    }

    /// Takes the union of every `restartComponent` flag word received since
    /// the last take, leaving zero behind. Wait-free and allocation-free, so
    /// it may run on an audio thread.
    pub fn take_restart_bits(&self) -> i32 {
        // Acquire pairs with the plugin's Release `fetch_or`: whatever the
        // plugin wrote before asking for the restart (a new latency, a new bus
        // layout) is visible to the host once it sees the bit.
        self.restart_bits.swap(0, Ordering::Acquire)
    }

    /// The oldest queued notification, if any. Lock-free and allocation-free,
    /// and dropping what it returns frees nothing.
    pub fn pop_notification(&self) -> Option<PluginNotification> {
        self.notifications.pop()
    }

    /// How many notifications are queued, unread.
    pub fn pending_notifications(&self) -> usize {
        self.notifications.len()
    }

    /// Whether any notification was dropped since the last take, clearing the
    /// flag. See [`NOTIFICATION_CAPACITY`] for what a caller does about it.
    pub fn take_overflowed(&self) -> bool {
        self.overflowed.swap(false, Ordering::Relaxed)
    }

    /// Notifications refused because the queue held
    /// [`NOTIFICATION_CAPACITY`] unread ones. Cumulative over the handler's
    /// life; a host polls it to learn that it read too rarely.
    pub fn dropped_notifications(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Queues `n`, or counts it if the queue is full. Never allocates and
    /// never blocks: the plugin may be calling from its process thread.
    fn push(&self, n: PluginNotification) {
        if self.notifications.push(n).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            self.overflowed.store(true, Ordering::Relaxed);
        }
    }

    fn edit(&self, e: ParameterEditEvent) {
        self.push(PluginNotification::Edit(e));
    }
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, id: ParamID) -> tresult {
        self.edit(ParameterEditEvent::BeginEdit(id));
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value_normalized: ParamValue) -> tresult {
        self.edit(ParameterEditEvent::PerformEdit {
            param_id: id,
            value: value_normalized,
        });
        kResultOk
    }

    unsafe fn endEdit(&self, id: ParamID) -> tresult {
        self.edit(ParameterEditEvent::EndEdit(id));
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        // Release pairs with `take_restart_bits`' Acquire; see there.
        self.restart_bits.fetch_or(flags, Ordering::Release);
        kResultOk
    }
}

impl IComponentHandler2Trait for ComponentHandler {
    unsafe fn setDirty(&self, state: TBool) -> tresult {
        self.edit(ParameterEditEvent::SetDirty(state != 0));
        kResultOk
    }

    unsafe fn requestOpenEditor(&self, _name: FIDString) -> tresult {
        self.edit(ParameterEditEvent::RequestOpenEditor);
        kResultOk
    }

    unsafe fn startGroupEdit(&self) -> tresult {
        self.edit(ParameterEditEvent::StartGroupEdit);
        kResultOk
    }

    unsafe fn finishGroupEdit(&self) -> tresult {
        self.edit(ParameterEditEvent::FinishGroupEdit);
        kResultOk
    }
}

impl IComponentHandler3Trait for ComponentHandler {
    /// Returns `null` to decline building a host context menu.
    ///
    /// This is intentional and spec-legal: `IComponentHandler3` lets a plugin
    /// ask the host for a menu it can populate with host-contributed items (and
    /// into which the plugin then injects its own), but returning `null` is the
    /// documented way to say "the host offers no menu here" — the plugin falls
    /// back to its own built-in menu. Declining is deliberate because nothing in
    /// the host or frontend contributes plugin context-menu items or consumes an
    /// `IContextMenu`; building a real host menu object would be dead surface. If
    /// a frontend consumer is ever added, this becomes a real `IContextMenu`
    /// implementation gated on that consumer.
    unsafe fn createContextMenu(
        &self,
        _plug_view: *mut IPlugView,
        _param_id: *const ParamID,
    ) -> *mut IContextMenu {
        std::ptr::null_mut()
    }
}

impl IComponentHandlerBusActivationTrait for ComponentHandler {
    unsafe fn requestBusActivation(
        &self,
        media_type: MediaType,
        dir: BusDirection,
        index: i32,
        state: TBool,
    ) -> tresult {
        self.edit(ParameterEditEvent::RequestBusActivation {
            media_type,
            direction: dir,
            index,
            state: state != 0,
        });
        kResultOk
    }
}

impl IProgressTrait for ComponentHandler {
    unsafe fn start(
        &self,
        r#type: ProgressType,
        optional_description: *const u16,
        out_id: *mut ID,
    ) -> tresult {
        let id = self.next_progress_id.fetch_add(1, Ordering::SeqCst);
        // SAFETY: the interface passes null or a NUL-terminated string.
        let desc = unsafe { ProgressDescription::from_utf16_nul(optional_description) };
        self.push(PluginNotification::Progress(ProgressEvent::Started {
            id,
            progress_type: r#type,
            description: desc,
        }));
        if !out_id.is_null() {
            *out_id = id;
        }
        kResultOk
    }

    unsafe fn update(&self, id: ID, norm_value: ParamValue) -> tresult {
        self.push(PluginNotification::Progress(ProgressEvent::Updated {
            id,
            progress: norm_value,
        }));
        kResultOk
    }

    unsafe fn finish(&self, id: ID) -> tresult {
        self.push(PluginNotification::Progress(ProgressEvent::Finished { id }));
        kResultOk
    }
}

impl IUnitHandlerTrait for ComponentHandler {
    unsafe fn notifyUnitSelection(&self, unit_id: UnitID) -> tresult {
        self.push(PluginNotification::Unit(UnitEvent::UnitSelected(unit_id)));
        kResultOk
    }

    unsafe fn notifyProgramListChange(
        &self,
        list_id: ProgramListID,
        program_index: i32,
    ) -> tresult {
        self.push(PluginNotification::Unit(UnitEvent::ProgramListChanged {
            list_id,
            program_index,
        }));
        kResultOk
    }
}

impl IUnitHandler2Trait for ComponentHandler {
    unsafe fn notifyUnitByBusChange(&self) -> tresult {
        self.push(PluginNotification::Unit(UnitEvent::UnitByBusChanged));
        kResultOk
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::Steinberg::Vst::RestartFlags_;

    /// Everything queued so far, oldest first.
    fn drain(handler: &ComponentHandler) -> Vec<PluginNotification> {
        std::iter::from_fn(|| handler.pop_notification()).collect()
    }

    /// The plugin's edit gestures must reach the host in order and carry their
    /// operands: a DAW writes automation from `PerformEdit` and opens/closes an
    /// undo transaction on the surrounding `Begin`/`EndEdit`.
    #[test]
    fn edit_gestures_forward_in_order_with_their_operands() {
        let handler = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

        unsafe {
            assert_eq!(ptr.beginEdit(42), kResultOk);
            assert_eq!(ptr.performEdit(42, 0.75), kResultOk);
            assert_eq!(ptr.endEdit(42), kResultOk);
        }

        let got = drain(&handler);
        assert_eq!(got.len(), 3);
        assert!(matches!(
            got[0],
            PluginNotification::Edit(ParameterEditEvent::BeginEdit(42))
        ));
        assert!(matches!(
            got[1],
            PluginNotification::Edit(ParameterEditEvent::PerformEdit { param_id: 42, value })
                if (value - 0.75).abs() < 1e-3
        ));
        assert!(matches!(
            got[2],
            PluginNotification::Edit(ParameterEditEvent::EndEdit(42))
        ));
    }

    /// `restartComponent` carries the raw flag word through unchanged — the
    /// host decodes it with [`RestartFlags::from_bits`], so swallowing or
    /// masking bits here would silently drop a latency or I/O change. It does
    /// not occupy the notification queue.
    #[test]
    fn restart_component_forwards_the_raw_flag_word() {
        let handler = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

        unsafe {
            assert_eq!(ptr.restartComponent(0b1010), kResultOk);
        }

        assert_eq!(handler.take_restart_bits(), 0b1010);
        assert!(handler.pop_notification().is_none());
    }

    /// Restart flags are cumulative: several requests between two takes reach
    /// the host as their union, and a take leaves nothing behind for the next.
    ///
    /// Mutation: `fetch_or` replaced by `store` in `restartComponent` (the
    /// last request wins) fails the union assertion — the latency change is
    /// lost under the later I/O change. `swap(0, ..)` replaced by `load` fails
    /// the "second take is empty" assertion.
    #[test]
    fn restart_flags_merge_across_calls_and_reset_on_take() {
        let handler = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

        unsafe {
            ptr.restartComponent(RestartFlags_::kLatencyChanged);
            ptr.restartComponent(RestartFlags_::kIoChanged);
            ptr.restartComponent(RestartFlags_::kLatencyChanged);
        }

        let flags = RestartFlags::from_bits(handler.take_restart_bits());
        assert!(flags.latency_changed, "the earlier request survives");
        assert!(flags.io_changed, "the later request lands too");
        assert!(!flags.param_values_changed);

        assert_eq!(handler.take_restart_bits(), 0, "a take consumes the flags");

        unsafe { ptr.restartComponent(RestartFlags_::kParamValuesChanged) };
        assert_eq!(
            handler.take_restart_bits(),
            RestartFlags_::kParamValuesChanged,
            "a request after a take starts from zero"
        );
    }

    /// A full queue drops the newest notification and counts it; it neither
    /// grows nor evicts what is already queued. The drop raises the sticky
    /// overflow flag, which one take reports and clears.
    ///
    /// Mutations: removing the `fetch_add` in `push` leaves the count at 0 and
    /// fails the first assertion; removing the `overflowed.store` fails the
    /// flag assertion; `take_overflowed` as a `load` (never cleared) fails
    /// the "reported once" assertion.
    #[test]
    fn overflow_is_counted_and_keeps_the_queued_events() {
        let handler = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

        let extra = 3;
        unsafe {
            for i in 0..(NOTIFICATION_CAPACITY + extra) as u32 {
                ptr.performEdit(i, 0.5);
            }
        }

        assert_eq!(handler.dropped_notifications(), extra as u64);
        assert_eq!(handler.pending_notifications(), NOTIFICATION_CAPACITY);
        assert!(handler.take_overflowed(), "a drop raises the flag");
        assert!(!handler.take_overflowed(), "a take reports it once");
        let got = drain(&handler);
        assert_eq!(got.len(), NOTIFICATION_CAPACITY);
        assert!(
            matches!(
                got[0],
                PluginNotification::Edit(ParameterEditEvent::PerformEdit { param_id: 0, .. })
            ),
            "the oldest event is kept; the overflow is the newest"
        );

        // Room again once drained; the count is cumulative, and a push that
        // fits does not raise the flag.
        unsafe { ptr.endEdit(7) };
        assert_eq!(drain(&handler).len(), 1);
        assert_eq!(handler.dropped_notifications(), extra as u64);
        assert!(!handler.take_overflowed());
    }

    /// The point of this module: a plugin calling the handler from its
    /// process thread must not allocate there. Every callback runs inside
    /// `assert_no_alloc`, `IProgress::start` with a description past the
    /// inline capacity included, as does a push into a full queue, and so
    /// does the host's side (`take_restart_bits`, `pop_notification`, and
    /// dropping what it pops) — which is what the plugin server's restart
    /// poll calls.
    ///
    /// Mutations: the queue swapped for `crossbeam_queue::SegQueue` (unbounded,
    /// allocating a block per 31 pushes — the growth the original
    /// `crossbeam_channel::unbounded` sender had) aborts inside the gate, and
    /// so does `ProgressDescription` holding a `String` (the old
    /// `utf16_to_string` decode).
    #[test]
    fn callbacks_and_the_host_drain_do_not_allocate() {
        let handler = ComponentHandler::new();
        let edit = handler.to_com_ptr::<IComponentHandler>().unwrap();
        let edit2 = handler.to_com_ptr::<IComponentHandler2>().unwrap();
        let bus = handler
            .to_com_ptr::<IComponentHandlerBusActivation>()
            .unwrap();
        let progress = handler.to_com_ptr::<IProgress>().unwrap();
        let unit = handler.to_com_ptr::<IUnitHandler>().unwrap();
        let unit2 = handler.to_com_ptr::<IUnitHandler2>().unwrap();
        // Longer than the inline capacity, so `start` also truncates.
        let long: Vec<u16> = std::iter::repeat_n(u16::from(b'x'), 300)
            .chain([0])
            .collect();

        assert_no_alloc::assert_no_alloc(|| unsafe {
            let mut id: ID = 0;
            progress.start(0, long.as_ptr(), &mut id);
            // Three edits per pass: two thirds of them meet a full queue.
            for i in 0..NOTIFICATION_CAPACITY as u32 {
                edit.beginEdit(i);
                edit.performEdit(i, 0.25);
                edit.endEdit(i);
                edit.restartComponent(RestartFlags_::kLatencyChanged);
            }
            edit2.setDirty(1);
            edit2.startGroupEdit();
            edit2.finishGroupEdit();
            edit2.requestOpenEditor(std::ptr::null());
            bus.requestBusActivation(0, 0, 0, 1);
            progress.update(1, 0.5);
            progress.finish(1);
            unit.notifyUnitSelection(1);
            unit.notifyProgramListChange(2, -1);
            unit2.notifyUnitByBusChange();

            assert_eq!(handler.take_restart_bits(), RestartFlags_::kLatencyChanged);
            let mut n = 0;
            while handler.pop_notification().is_some() {
                n += 1;
            }
            assert_eq!(n, NOTIFICATION_CAPACITY);
        });
        assert!(handler.dropped_notifications() > 0, "the full path ran");
    }

    #[test]
    fn restart_flags_decode_individual_bits() {
        let latency = RestartFlags::from_bits(RestartFlags_::kLatencyChanged);
        assert!(latency.latency_changed);
        assert!(!latency.io_changed);
        assert!(!latency.param_values_changed);

        let io = RestartFlags::from_bits(RestartFlags_::kIoChanged);
        assert!(io.io_changed);
        assert!(!io.latency_changed);

        assert!(
            RestartFlags::from_bits(RestartFlags_::kMidiCCAssignmentChanged)
                .midi_cc_assignment_changed
        );
        assert!(RestartFlags::from_bits(RestartFlags_::kReloadComponent).reload_component);
        assert!(RestartFlags::from_bits(RestartFlags_::kParamTitlesChanged).param_titles_changed);
        assert!(RestartFlags::from_bits(RestartFlags_::kParamValuesChanged).param_values_changed);
    }

    #[test]
    fn restart_flags_decode_combined_bits() {
        let flags = RestartFlags::from_bits(
            RestartFlags_::kLatencyChanged
                | RestartFlags_::kIoChanged
                | RestartFlags_::kParamValuesChanged,
        );
        assert!(flags.latency_changed);
        assert!(flags.io_changed);
        assert!(flags.param_values_changed);
        assert!(!flags.param_titles_changed);
        assert!(!flags.reload_component);
    }

    #[test]
    fn restart_flags_decode_zero_is_empty() {
        assert_eq!(RestartFlags::from_bits(0), RestartFlags::default());
    }

    /// `IProgress::start` mints the id the plugin then quotes back on
    /// `update`/`finish`, so the id must be returned through the out-param
    /// *and* carried on the event — a host that loses it cannot match a
    /// progress bar to its completion.
    #[test]
    fn progress_start_mints_an_id_that_update_and_finish_quote_back() {
        let handler = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IProgress>().unwrap();

        let out_id = unsafe {
            let mut out_id: ID = 0;
            let desc: [u16; 5] = [b'T' as u16, b'e' as u16, b's' as u16, b't' as u16, 0];
            assert_eq!(ptr.start(1, desc.as_ptr(), &mut out_id), kResultOk);
            assert_ne!(out_id, 0, "start must mint a non-zero id");

            assert_eq!(ptr.update(out_id, 0.5), kResultOk);
            assert_eq!(ptr.finish(out_id), kResultOk);
            out_id
        };

        let mut got = drain(&handler).into_iter();
        match got.next() {
            Some(PluginNotification::Progress(ProgressEvent::Started {
                id,
                progress_type,
                description,
            })) => {
                assert_eq!(id, out_id);
                assert_eq!(progress_type, 1);
                assert_eq!(description.as_str(), "Test");
            }
            other => panic!("expected Started, got {other:?}"),
        }
        match got.next() {
            Some(PluginNotification::Progress(ProgressEvent::Updated { id, progress })) => {
                assert_eq!(id, out_id);
                assert!((progress - 0.5).abs() < 1e-3);
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        assert!(matches!(
            got.next(),
            Some(PluginNotification::Progress(ProgressEvent::Finished { id })) if id == out_id
        ));
    }

    /// A description longer than the inline capacity is cut at the last whole
    /// character that fits, never mid-character; unpaired surrogates become
    /// U+FFFD rather than ending the decode.
    ///
    /// Mutations: `break` → `continue` on a character that does not fit
    /// (packing a later, smaller one into the gap) fails the length
    /// assertion; skipping an unpaired surrogate instead of replacing it
    /// fails the prefix assertion.
    #[test]
    fn a_long_progress_description_is_truncated_at_a_char_boundary() {
        // An unpaired high surrogate, then 'é' (2 bytes) and '€' (3 bytes)
        // repeated well past the capacity.
        let mut utf16: Vec<u16> = vec![0xD800, u16::from(b'a')];
        for _ in 0..100 {
            utf16.push(0x00E9);
            utf16.push(0x20AC);
        }
        utf16.push(0);
        let d = unsafe { ProgressDescription::from_utf16_nul(utf16.as_ptr()) };
        assert!(d.as_str().starts_with("\u{FFFD}a\u{e9}\u{20ac}"));
        assert!(d.len() <= PROGRESS_DESCRIPTION_CAPACITY);
        // Byte budget 128: "\u{FFFD}a" is 4, then 5-byte pairs. 4 + 24*5 = 124,
        // and the next 'é' (2) fits at 126 while '€' (3) would reach 129.
        assert_eq!(d.len(), 126, "filled to the last whole character");
        assert!(d.as_str().ends_with('\u{e9}'));
    }

    /// A null description is legal per the interface (`optionalDescription`);
    /// the handler must read it as empty rather than dereferencing it.
    #[test]
    fn progress_start_accepts_a_null_description() {
        let handler = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IProgress>().unwrap();

        unsafe {
            let mut out_id: ID = 0;
            assert_eq!(ptr.start(0, std::ptr::null(), &mut out_id), kResultOk);
        }

        match handler.pop_notification() {
            Some(PluginNotification::Progress(ProgressEvent::Started { description, .. })) => {
                assert_eq!(description.as_str(), "")
            }
            other => panic!("expected Started, got {other:?}"),
        }
    }

    #[test]
    fn unit_selection_and_program_list_changes_forward_their_ids() {
        let handler = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IUnitHandler>().unwrap();

        unsafe {
            assert_eq!(ptr.notifyUnitSelection(5), kResultOk);
            assert_eq!(ptr.notifyProgramListChange(10, 3), kResultOk);
        }

        let got = drain(&handler);
        assert!(matches!(
            got[0],
            PluginNotification::Unit(UnitEvent::UnitSelected(5))
        ));
        match got[1] {
            PluginNotification::Unit(UnitEvent::ProgramListChanged {
                list_id,
                program_index,
            }) => {
                assert_eq!(list_id, 10);
                assert_eq!(program_index, 3);
            }
            ref other => panic!("expected ProgramListChanged, got {other:?}"),
        }
    }

    /// The three kinds share one queue, so their relative order survives: a
    /// unit change the plugin made *before* an edit is read before it.
    ///
    /// Mutation: giving progress its own queue (drained after edits, as the
    /// three separate channels were) puts `Progress` last and fails this.
    #[test]
    fn notifications_keep_their_order_across_kinds() {
        let handler = ComponentHandler::new();
        let edit = handler.to_com_ptr::<IComponentHandler>().unwrap();
        let progress = handler.to_com_ptr::<IProgress>().unwrap();
        let unit = handler.to_com_ptr::<IUnitHandler>().unwrap();

        unsafe {
            unit.notifyUnitSelection(1);
            progress.update(9, 0.5);
            edit.performEdit(3, 0.1);
        }

        let got = drain(&handler);
        assert!(matches!(got[0], PluginNotification::Unit(_)));
        assert!(matches!(got[1], PluginNotification::Progress(_)));
        assert!(matches!(got[2], PluginNotification::Edit(_)));
    }

    /// The handler is shared with the plugin's GUI thread as a bare COM
    /// pointer, so concurrent `performEdit` calls must all land: an event
    /// dropped under contention is an automation write the DAW never sees.
    #[test]
    #[cfg_attr(miri, ignore = "threaded stress test")]
    fn concurrent_edits_all_reach_the_host() {
        let handler = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();
        let ptr_addr = ptr.as_ptr() as usize;

        let handles: Vec<_> = (0..4)
            .map(|i| {
                std::thread::spawn(move || unsafe {
                    let r =
                        vst3::ComRef::<IComponentHandler>::from_raw_unchecked(ptr_addr as *mut _);
                    for j in 0..10 {
                        r.performEdit(i as u32, j as f64 * 0.1);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(drain(&handler).len(), 40);
        assert_eq!(handler.dropped_notifications(), 0);

        drop(ptr);
        drop(handler);
    }
}
