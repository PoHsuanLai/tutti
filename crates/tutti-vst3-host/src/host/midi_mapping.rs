//! VST3 `IMidiMapping` support: the CC→parameter routing table.
//!
//! Many VST3 instruments expose mod-wheel, breath, expression, sustain, etc.
//! *only* through `IMidiMapping::getMidiControllerAssignment` — the spec's
//! intended path — rather than reacting to raw CC `Data` events. Without this
//! query those controllers silently do nothing.
//!
//! At load (UI thread) the host walks every channel × controller-number slot and asks
//! the controller which parameter, if any, that controller drives. The result
//! is a flat `[16 channels][kCountCtrlNumber]` table of `ParamID`s, sentinel
//! [`NO_PARAM_ID`] where unmapped. At process time the audio path looks each
//! incoming CC/aftertouch/pitch-bend up and, when mapped, routes it as a
//! parameter-change point (normalized to 0..1) instead of (only) a MIDI event.
//!
//! `IMidiMapping` is an extension of `IEditController`, which the spec says is
//! a UI-thread object — so the table is built once off the audio thread and
//! only *read* (lock-free, allocation-free) on the audio thread.
//!
//! Spec reference: `Steinberg::Vst::IMidiMapping`,
//! `Steinberg::Vst::ControllerNumbers`.

use vst3::Steinberg::Vst::{
    ControllerNumbers_::{kAfterTouch, kCountCtrlNumber, kPitchBend},
    ParamID,
};

use tutti_midi_protocol::widen;
use tutti_ump::{Message, Ump, Unknown, Voice2, decode};

use crate::events::{InputEvents, Stager};
use crate::types::{MidiEvent, ParameterChanges};
use tutti_plugin_types::{MIDI_STACK_CAPACITY, MainThread};
use tutti_rt::{Reader, RtVec};
use tutti_vst3_ffi::{EventList, Plugin};

/// The MIDI a [`CcRoute`] passes on: a block's whole MIDI cap, so the route
/// never drops an event the plugin node delivered. Dropping one would cut a
/// multi-packet SysEx the event list is reassembling.
pub(crate) type FilteredMidi = RtVec<MidiEvent, MIDI_STACK_CAPACITY>;

/// Number of MIDI channels VST3 enumerates mappings for.
pub(crate) const NUM_CHANNELS: usize = 16;

/// Number of distinct controller slots per channel: CC 0-127 plus the two
/// synthetic controllers (`kAfterTouch` = 128, `kPitchBend` = 129).
/// Equal to `Steinberg::Vst::kCountCtrlNumber`.
pub(crate) const NUM_CONTROLLERS: usize = kCountCtrlNumber as usize;

/// Sentinel `ParamID` meaning "no parameter mapped". Mirrors
/// `Steinberg::Vst::kNoParamId` (`0xFFFF_FFFF`).
pub(crate) const NO_PARAM_ID: ParamID = u32::MAX;

/// Synthetic controller index VST3 uses for channel aftertouch.
pub(crate) const CTRL_AFTERTOUCH: usize = kAfterTouch as usize;
/// Synthetic controller index VST3 uses for pitch bend.
pub(crate) const CTRL_PITCH_BEND: usize = kPitchBend as usize;

/// A [`MidiCcMapping`] as the control thread publishes it to the Processor.
pub(crate) struct MidiCcCell(pub(crate) MidiCcMapping);

/// Per-channel CC→`ParamID` table queried from `IMidiMapping`.
///
/// Lookups are infallible and allocation-free: out-of-range channels or
/// controllers (or unmapped slots) return `None`. An empty table (no
/// `IMidiMapping`, or no mappings at all) makes [`is_empty`](Self::is_empty)
/// true so the audio path can skip the routing pass entirely.
pub(crate) struct MidiCcMapping {
    /// `table[channel * NUM_CONTROLLERS + controller]`. Flat to keep it on one
    /// heap allocation and cache-friendly for the per-event lookup.
    table: Vec<ParamID>,
    /// True if at least one slot is mapped — lets the hot path bail fast.
    any_mapped: bool,
}

impl MidiCcMapping {
    /// An all-unmapped table. Used when the plugin exposes no `IMidiMapping`.
    pub(crate) fn empty() -> Self {
        Self {
            table: Vec::new(),
            any_mapped: false,
        }
    }

    /// Queries the plugin's `IMidiMapping` (if its controller exposes one) for
    /// every channel × controller slot and builds the table.
    ///
    /// Returns an [`empty`](Self::empty) table when the controller is absent
    /// or doesn't implement `IMidiMapping`. Must run on the UI/main thread —
    /// `IMidiMapping` is an `IEditController` extension.
    pub(crate) fn query(main: &MainThread, plugin: &Plugin) -> Self {
        if !plugin.has_midi_mapping(main) {
            return Self::empty();
        }

        let mut table = vec![NO_PARAM_ID; NUM_CHANNELS * NUM_CONTROLLERS];
        let mut any_mapped = false;
        // busIndex 0: CC mappings are reported against the first event bus
        // (matches the JUCE host contract).
        plugin.midi_controller_assignments(
            main,
            0..NUM_CHANNELS as i16,
            0..NUM_CONTROLLERS as i16,
            |channel, controller_number, id| {
                if id != NO_PARAM_ID {
                    table[channel as usize * NUM_CONTROLLERS + controller_number as usize] = id;
                    any_mapped = true;
                }
            },
        );

        if any_mapped {
            Self { table, any_mapped }
        } else {
            // Nothing mapped — drop the allocation, behave as empty.
            Self::empty()
        }
    }

    /// True if no controller on any channel maps to a parameter. The audio
    /// path skips CC→param routing entirely when this holds.
    pub(crate) fn is_empty(&self) -> bool {
        !self.any_mapped
    }

    /// Looks up the `ParamID` a given channel/controller drives, or `None` if
    /// the slot is unmapped or out of range.
    ///
    /// `controller` is a VST3 `ControllerNumbers` index: 0-127 for standard
    /// CCs, [`CTRL_AFTERTOUCH`] (128) for channel pressure, [`CTRL_PITCH_BEND`]
    /// (129) for pitch bend.
    pub(crate) fn lookup(&self, channel: u8, controller: usize) -> Option<ParamID> {
        if self.table.is_empty() {
            return None;
        }
        let channel = channel as usize;
        if channel >= NUM_CHANNELS || controller >= NUM_CONTROLLERS {
            return None;
        }
        let id = self.table[channel * NUM_CONTROLLERS + controller];
        (id != NO_PARAM_ID).then_some(id)
    }
}

/// Decompose a [`MidiEvent`] into the `(channel, controller, normalized_value)`
/// a VST3 `IMidiMapping` parameter expects, or `None` if the event is not a
/// mappable controller (CC / channel pressure / pitch bend).
///
/// The packet is read through [`decode`]; a MIDI 1.0 message is widened
/// ([`widen`]) so MIDI-2 controllers keep their full width here. The group is
/// ignored: `IMidiMapping` is addressed by bus and channel only. Values are
/// normalized to the VST3 host contract at this edge:
/// - CC / channel pressure: 32-bit → unit `0.0..=1.0`.
/// - Pitch bend: signed `-1.0..=1.0` (center `0.0`) → `0.0..=1.0` (center
///   `0.5`), matching what the plugin's parameter funnel expects.
pub(crate) fn midi_to_mapped_controller(event: &MidiEvent) -> Option<(u8, usize, f64)> {
    use tutti_midi_types::convert::{bend_u32_to_signed_f32, u32_to_unit_f32};

    let (channel, voice) = match decode(Ump(event.data)) {
        Ok(Message::Midi1(address, voice)) => (address.channel, widen(voice)),
        Ok(Message::Midi2(address, voice)) => (address.channel, voice),
        // Not channel voice, or a packet `decode` cannot frame: there is no
        // controller in it, and the caller keeps the event as MIDI.
        Ok(
            Message::Utility(_)
            | Message::System(..)
            | Message::Sysex7(..)
            | Message::Data128(..)
            | Message::Flex(..)
            | Message::Stream(_),
        )
        | Err(Unknown(_)) => return None,
    };
    let channel = channel.get();
    match voice {
        Voice2::ControlChange { controller, value } => Some((
            channel,
            usize::from(controller.get()),
            f64::from(u32_to_unit_f32(value)),
        )),
        Voice2::ChannelPressure(pressure) => Some((
            channel,
            CTRL_AFTERTOUCH,
            f64::from(u32_to_unit_f32(pressure)),
        )),
        Voice2::PitchBend(bend) => {
            // Signed [-1, 1] (center 0) → unit [0, 1] (center 0.5).
            let value = bend_u32_to_signed_f32(bend);
            Some((channel, CTRL_PITCH_BEND, (f64::from(value) + 1.0) / 2.0))
        }
        // No `IMidiMapping` slot: the caller keeps these as MIDI.
        Voice2::RegisteredPerNote { .. }
        | Voice2::AssignablePerNote { .. }
        | Voice2::Registered { .. }
        | Voice2::Assignable { .. }
        | Voice2::RelativeRegistered { .. }
        | Voice2::RelativeAssignable { .. }
        | Voice2::PerNotePitchBend { .. }
        | Voice2::NoteOff { .. }
        | Voice2::NoteOn { .. }
        | Voice2::PolyPressure { .. }
        | Voice2::ProgramChange { .. }
        | Voice2::PerNoteManagement { .. } => None,
    }
}

/// Splits `midi_events` against `mapping`: mapped CC/aftertouch/pitch-bend
/// messages become normalized parameter points appended to `out_params`
/// (already seeded with the host's automation); everything else is pushed to
/// `out_filtered` to remain a MIDI event.
///
/// The caller owns clearing/seeding `out_params` and clearing `out_filtered`
/// before the call (the audio path clears in place to stay allocation-free);
/// this routine only appends. Decoding goes through
/// [`midi_to_mapped_controller`], so MIDI-2 controllers keep their full width.
///
/// Allocation-free given pre-warmed `out_*` capacity — the inner loop only
/// pushes into existing storage.
pub(crate) fn route_cc_events(
    mapping: &MidiCcMapping,
    midi_events: &[MidiEvent],
    out_filtered: &mut FilteredMidi,
    out_params: &mut ParameterChanges,
) {
    for event in midi_events {
        let mapped = midi_to_mapped_controller(event);
        match mapped {
            Some((channel, controller, value)) => match mapping.lookup(channel, controller) {
                Some(param_id) => {
                    // `IMidiMapping` answers with a `ParamID` — opaque, like
                    // every VST3 parameter address. Past the queue's room it
                    // keeps the latest value in time, the host's or the CC's
                    // (`PointBuf::push_or_replace_last`), counted in the
                    // set's `points_overflowed`.
                    let _ =
                        out_params.add_change(param_id.into(), event.frame_offset as i32, value);
                }
                // Mappable message but no mapping for this slot — keep it as a
                // MIDI event so the plugin can still react.
                //
                // A refused push (more than a block's MIDI cap in) drops the
                // event rather than allocating on the audio thread;
                // `out_filtered.is_overflowed()` reports it to the caller.
                None => {
                    let _ = out_filtered.push(*event);
                }
            },
            // Not a mappable controller (note on/off, program change, …).
            None => {
                let _ = out_filtered.push(*event);
            }
        }
    }
}

/// Stage a block's input events into `list`, the MIDI through `cc` first
/// (mapped controllers become parameter changes), and return the parameter
/// changes to forward: the route's merged ones, or `param_changes` when the
/// plugin maps nothing. When the route dropped MIDI past its cap, the list
/// drops its open SysEx ([`EventList::lose_sysex_tail`]) so a message never
/// reaches the plugin with a gap in it. Never allocates.
///
/// `None` when the list was busy (a plugin thread inside `getEventCount` or
/// `getEvent`) and could not be staged or cleared: it still holds the last
/// block's events, so the block must not be processed.
pub(crate) fn stage_input_events<'a>(
    cc: &'a mut CcRoute,
    stager: &mut Stager,
    list: &EventList,
    events: &InputEvents<'_>,
    param_changes: Option<&'a ParameterChanges>,
) -> Option<Option<&'a ParameterChanges>> {
    let (midi, params, lost_tail) = match cc.route(events.midi, param_changes) {
        Some((midi, params)) => (midi.as_slice(), Some(params), midi.is_overflowed()),
        None => (events.midi, param_changes, false),
    };
    let effective = InputEvents { midi, ..*events };
    let staged = if effective.is_empty() {
        list.clear()
    } else {
        stager.stage(list, &effective)
    };
    if !staged {
        return None;
    }
    if lost_tail {
        list.lose_sysex_tail();
    }
    Some(params)
}

/// Sorts every queue's points into ascending `sample_offset` order, as VST3's
/// `IParamValueQueue` requires. Host automation is seeded before CC-derived
/// points are appended (in MIDI-arrival order), so a queue carrying both can be
/// unsorted even when each source was individually ordered. In place — each
/// queue's points keep their storage, so no queue grows.
///
/// The sort must be **stable**. VST3 encodes a discontinuity as two points —
/// the old value, then the new one — and permits them to share a
/// `sample_offset`. Their relative order is the entire encoding: transposed,
/// a jump up reads as a jump down. An unstable sort is free to swap equal
/// keys, so it silently corrupts exactly the case the two-point form exists
/// to express, and only when both sources land on the same frame.
///
/// It must also **not allocate**: it runs on the audio thread, and std's
/// stable `sort_by_key` allocates a scratch buffer past 256 points, which a
/// queue at a chunk's room exceeds at every chunk size
/// (`ParameterQueue::chunk_room` is `ceil(chunk / 32) + 1 + 256`).
/// `PointBuf::sort_by_offset` is a stable insertion sort in place: linear on
/// a queue already in order (every queue with one source), and quadratic in
/// the overlap of the runs otherwise (up to n1 * n2 moves for the host's run
/// and the CCs' interleaved), bounded by the queue's room.
pub(crate) fn sort_param_points(params: &mut ParameterChanges) {
    for queue in params.queues.iter_mut() {
        queue.points.sort_by_offset();
    }
}

/// The `IMidiMapping` CC→param routing concern as a per-block unit: the
/// controller-queried mapping table plus the pooled scratch the routing pass
/// needs. Held by the Processor's `AudioIO`.
///
/// When `mapping` is non-empty, mapped CC/aftertouch/pitch-bend events are
/// pulled out of the input MIDI into `filtered_midi` (the events that still
/// reach the plugin's event list) while their parameter points are merged into
/// `param_changes` alongside the host's automation. The scratch buffers are
/// reused across blocks to keep the routing pass allocation-free; `mapping` is
/// rebuilt only when the plugin signals `kMidiCCAssignmentChanged` (see
/// [`Vst3Loaded::rebuild_midi_cc_mapping`](crate::Vst3Loaded::rebuild_midi_cc_mapping))
/// and published to a route built [`following`](CcRoute::following) it.
///
/// **`param_changes` is reserved exactly while `mapping` is non-empty.** The
/// reservation is the layout the host's chunk arrives in
/// (`ParameterChanges::reserved`, `point_budget(queues, room)`: up to 16448
/// queues of a few points each and a shared pool of a block's chunk room and
/// one more, about a megabyte at a large block; past it a point gives way,
/// counted, and nothing grows), and `route` never touches
/// the scratch for an empty mapping, so a plugin with no CC mapping (most of
/// them) holds none of it. `mapping` is private so that the only ways to
/// change it, [`new`](Self::new) and [`set_mapping`](Self::set_mapping), both
/// run off the audio thread and keep the two in step: `route` never sees a
/// non-empty mapping over unreserved scratch.
pub(crate) struct CcRoute {
    mapping: MidiCcMapping,
    /// The mapping the control thread publishes, lent once per block. When
    /// set it is the mapping routed against and `mapping` is unused.
    shared: Option<Reader<MidiCcCell>>,
    filtered_midi: FilteredMidi,
    param_changes: ParameterChanges,
    /// The parameter queues to reserve (the host's chunk's: the plugin's
    /// parameter count and the spare ones).
    queues: usize,
    /// The chunk room the scratch is reserved for (the host input list's:
    /// `ParameterQueue::chunk_room` at the activated block size).
    room: usize,
    /// Points the scratch's own count does not hold (see
    /// [`points_overflowed`](Self::points_overflowed)): those of earlier
    /// scratches, kept across a [`set_mapping`](Self::set_mapping) that replaces the
    /// scratch, and the host's points the seed could not copy.
    overflowed: u64,
}

impl CcRoute {
    /// Builds with `mapping` and empty scratch. The MIDI scratch holds a
    /// block's MIDI cap ([`MIDI_STACK_CAPACITY`]) and never grows. When `mapping` is non-empty, `param_changes` is
    /// reserved with `queues` parameter queues (the host's chunk's) for a
    /// chunk room of `room` (the host input list's) in the layout
    /// (`ParameterChanges::reserved`: a floor of `FLOOR_POINTS + 1` each and
    /// a shared pool of blocks of `room + 1`, as the host's chunk arrives,
    /// carried values and all), and never grows:
    /// past it a point gives way to its parameter's latest value, counted
    /// ([`points_overflowed`](Self::points_overflowed)); when it is empty,
    /// `param_changes` holds nothing until [`set_mapping`](Self::set_mapping)
    /// installs a non-empty one. Allocates; call it off the audio thread.
    pub(crate) fn new(mapping: MidiCcMapping, queues: usize, room: usize) -> Self {
        let mut route = Self {
            mapping: MidiCcMapping::empty(),
            shared: None,
            filtered_midi: FilteredMidi::new(),
            param_changes: ParameterChanges::new(),
            queues,
            room,
            overflowed: 0,
        };
        route.set_mapping(mapping);
        route
    }

    /// Builds a route that follows a published mapping: the mapping lent at
    /// each block is the one routed against. The param scratch is reserved
    /// now, whatever the mapping holds, because a later publication can make
    /// it non-empty and the audio thread cannot reserve it then; pass
    /// `reserve: false` for a plugin that has no `IMidiMapping` to publish.
    /// Allocates; call it off the audio thread.
    pub(crate) fn following(
        reader: Reader<MidiCcCell>,
        reserve: bool,
        queues: usize,
        room: usize,
    ) -> Self {
        let mut route = Self::new(MidiCcMapping::empty(), queues, room);
        if reserve {
            route.param_changes = ParameterChanges::reserved(queues, room);
        }
        route.shared = Some(reader);
        route
    }

    /// Points the merged host-automation and CC changes could not hold
    /// whole since the route was built: past a queue's room with the
    /// scratch's pool spent (the queue kept the parameter's latest value,
    /// host or CC, whichever is later), or for a parameter past every
    /// reserved queue (dropped). Zero while the host's chunk and the
    /// mapped controllers stay within the reservation.
    pub(crate) fn points_overflowed(&self) -> u64 {
        self.overflowed + self.param_changes.points_overflowed()
    }

    /// Installs `mapping`, reserving the param scratch when it is non-empty
    /// and the scratch is not reserved yet, and releasing it when `mapping`
    /// is empty. A scratch already reserved is kept as it was reserved.
    /// Allocates or frees; call it off the audio thread (the `&mut self`
    /// already excludes a concurrent `route`).
    pub(crate) fn set_mapping(&mut self, mapping: MidiCcMapping) {
        let replaced = if mapping.is_empty() {
            Some(ParameterChanges::new())
        } else if self.mapping.is_empty() {
            Some(ParameterChanges::reserved(self.queues, self.room))
        } else {
            None
        };
        if let Some(scratch) = replaced {
            let old = core::mem::replace(&mut self.param_changes, scratch);
            self.overflowed += old.points_overflowed();
        }
        self.mapping = mapping;
    }

    /// Route `IMidiMapping`-mapped controllers into parameter changes, returning
    /// the MIDI + params the plugin should actually receive this block.
    ///
    /// When the mapping is non-empty, walks `midi_events` and, for each mapped
    /// CC / channel-pressure / pitch-bend message, appends a normalized
    /// parameter point to the scratch `param_changes` (seeded with the host's
    /// `param_changes`) and *omits* that event from the scratch `filtered_midi`.
    /// Unmapped events (notes, unmapped CCs, …) pass through unchanged. Returns
    /// `Some((filtered_midi, merged_params))` borrowing the scratch.
    ///
    /// More than [`MIDI_STACK_CAPACITY`] events in drop the rest, which the
    /// returned list's `is_overflowed()` reports; the plugin node never
    /// delivers more.
    ///
    /// Returns `None` when the plugin has no mapping — the common case for
    /// effects and simple instruments, so the caller forwards its inputs
    /// untouched and the hot path pays nothing.
    ///
    /// Allocation-free after warmup: both scratch buffers are cleared in place
    /// and reuse their heap capacity. Decoding goes through MIDI-1 bytes, the
    /// same lossless path the event list's encoding uses for events.
    pub(crate) fn route(
        &mut self,
        midi_events: &[MidiEvent],
        param_changes: Option<&ParameterChanges>,
    ) -> Option<(&FilteredMidi, &ParameterChanges)> {
        let Self {
            mapping,
            shared,
            filtered_midi,
            param_changes: merged,
            overflowed,
            ..
        } = self;
        // Lent once per block, and let go when this call returns.
        let loan = shared.as_mut().map(|reader| reader.lend());
        let mapping: &MidiCcMapping = match &loan {
            Some(loan) => &loan.0,
            None => mapping,
        };
        if mapping.is_empty() {
            return None;
        }

        // Clear scratch in place: the param queues go back to the set's
        // spares with their point storage, so the next block reuses them.
        filtered_midi.clear();
        merged.clear();

        // Seed the merged param changes with the host's automation, a
        // queue opened for each of the host's (its addresses are distinct),
        // so the copy is linear in the chunk. The layout fits a floor queue
        // carrying the bridge's carried value in its own room, so the pool's
        // blocks go to the queues at full room wherever they are in the
        // chunk. A queue past the reservation is dropped, counted.
        if let Some(pc) = param_changes {
            let changes = &mut *merged;
            for queue in &pc.queues {
                if !changes.open_queue(queue.param_id) {
                    *overflowed += queue.points.len() as u64;
                    continue;
                }
                let i = changes.queues.len() - 1;
                for point in &queue.points {
                    let stored = changes.push_point(i, point.sample_offset, point.value.get());
                    *overflowed += u64::from(!stored);
                }
            }
        }

        route_cc_events(mapping, midi_events, filtered_midi, merged);

        // VST3 requires each IParamValueQueue's points in ascending
        // sampleOffset order; seeding + appending can leave them unsorted.
        sort_param_points(merged);

        Some((&*filtered_midi, &*merged))
    }
}

#[cfg(test)]
mod tests;
