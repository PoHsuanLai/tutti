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

use vst3::ComPtr;
use vst3::Steinberg::kResultTrue;
use vst3::Steinberg::Vst::{
    ControllerNumbers_::{kAfterTouch, kCountCtrlNumber, kPitchBend},
    IEditController, IMidiMapping, IMidiMappingTrait, ParamID,
};

use crate::types::{MidiEvent, ParameterChanges};
use tutti_plugin_types::{ParamAddress, RtMidiEvents};

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

    /// Queries the controller's `IMidiMapping` (if it exposes one) for every
    /// channel × controller slot and build the table.
    ///
    /// Returns an [`empty`](Self::empty) table when the controller is absent
    /// or doesn't implement `IMidiMapping`. Must run on the UI/main thread —
    /// `IMidiMapping` is an `IEditController` extension.
    pub(crate) fn query(controller: Option<&ComPtr<IEditController>>) -> Self {
        let Some(mapping) = controller.and_then(|c| c.cast::<IMidiMapping>()) else {
            return Self::empty();
        };

        let mut table = vec![NO_PARAM_ID; NUM_CHANNELS * NUM_CONTROLLERS];
        let mut any_mapped = false;
        for channel in 0..NUM_CHANNELS {
            for controller_number in 0..NUM_CONTROLLERS {
                let mut id: ParamID = NO_PARAM_ID;
                // busIndex 0: CC mappings are reported against the first event
                // bus (matches the JUCE host contract).
                let rc = unsafe {
                    mapping.getMidiControllerAssignment(
                        0,
                        channel as i16,
                        controller_number as i16,
                        &mut id,
                    )
                };
                if rc == kResultTrue && id != NO_PARAM_ID {
                    table[channel * NUM_CONTROLLERS + controller_number] = id;
                    any_mapped = true;
                }
            }
        }

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
/// The event is matched on `midi2`'s Channel Voice 2 vocabulary via
/// [`tutti_midi_types::normalize`], so MIDI-2 controllers keep their full width
/// here; values are normalized to the VST3 host contract at this edge:
/// - CC / channel pressure: 32-bit → unit `0.0..=1.0`.
/// - Pitch bend: signed `-1.0..=1.0` (center `0.0`) → `0.0..=1.0` (center
///   `0.5`), matching what the plugin's parameter funnel expects.
pub(crate) fn midi_to_mapped_controller(event: &MidiEvent) -> Option<(u8, usize, f64)> {
    use tutti_midi_types::convert::{bend_u32_to_signed_f32, u32_to_unit_f32};
    use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
    use tutti_midi_types::midi2::{Channeled, UmpMessage};

    let normalized = tutti_midi_types::normalize(event);
    let UmpMessage::ChannelVoice2(cv2) = UmpMessage::try_from(normalized.data_words()).ok()? else {
        return None;
    };
    let channel = u8::from(cv2.channel());
    match cv2 {
        Cv2::ControlChange(m) => Some((
            channel,
            u8::from(m.control()) as usize,
            f64::from(u32_to_unit_f32(m.control_change_data())),
        )),
        Cv2::ChannelPressure(m) => Some((
            channel,
            CTRL_AFTERTOUCH,
            f64::from(u32_to_unit_f32(m.channel_pressure_data())),
        )),
        Cv2::ChannelPitchBend(m) => {
            // Signed [-1, 1] (center 0) → unit [0, 1] (center 0.5).
            let value = bend_u32_to_signed_f32(m.pitch_bend_data());
            Some((channel, CTRL_PITCH_BEND, (f64::from(value) + 1.0) / 2.0))
        }
        _ => None,
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
/// [`tutti_midi_types::decode`], so MIDI-2 controllers keep their full width.
///
/// Allocation-free given pre-warmed `out_*` capacity — the inner loop only
/// pushes into existing storage.
pub(crate) fn route_cc_events(
    mapping: &MidiCcMapping,
    midi_events: &[MidiEvent],
    out_filtered: &mut RtMidiEvents,
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
                    let _ = out_params.add_change(
                        ParamAddress::Opaque(param_id.into()),
                        event.frame_offset as i32,
                        value,
                    );
                }
                // Mappable message but no mapping for this slot — keep it as a
                // MIDI event so the plugin can still react.
                //
                // A refused push (pool at capacity) drops the event rather
                // than allocating on the audio thread; `out_filtered
                // .overflowed()` reports it to the caller.
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
/// queue at a chunk's room (`ParameterQueue::chunk_room`, 273 at 512 frames)
/// exceeds. `PointBuf::sort_by_offset` is a stable insertion sort in place:
/// linear on a queue already in order (every queue with one source), and
/// cheap on the few CC points appended out of place.
pub(crate) fn sort_param_points(params: &mut ParameterChanges) {
    for queue in params.queues.iter_mut() {
        queue.points.sort_by_offset();
    }
}

/// The `IMidiMapping` CC→param routing concern as a per-block unit: the
/// controller-queried mapping table plus the pooled scratch the routing pass
/// needs. Held by `Vst3Active`'s `AudioIO`.
///
/// When `mapping` is non-empty, mapped CC/aftertouch/pitch-bend events are
/// pulled out of the input MIDI into `filtered_midi` (the events that still
/// reach the plugin's event list) while their parameter points are merged into
/// `param_changes` alongside the host's automation. The scratch buffers are
/// reused across blocks to keep the routing pass allocation-free; `mapping` is
/// rebuilt only when the plugin signals `kMidiCCAssignmentChanged` (see
/// [`Vst3Active::rebuild_midi_cc_mapping`](crate::Vst3Active)).
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
    filtered_midi: RtMidiEvents,
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
    /// Builds with `mapping` and empty scratch. The MIDI scratch is capped
    /// and never grows. When `mapping` is non-empty, `param_changes` is
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
            filtered_midi: RtMidiEvents::new(),
            param_changes: ParameterChanges::new(),
            queues,
            room,
            overflowed: 0,
        };
        route.set_mapping(mapping);
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
    /// Returns `None` when the plugin has no mapping — the common case for
    /// effects and simple instruments, so the caller forwards its inputs
    /// untouched and the hot path pays nothing.
    ///
    /// Allocation-free after warmup: both scratch buffers are cleared in place
    /// and reuse their heap capacity. Decoding goes through MIDI-1 bytes, the
    /// same lossless path `Vst3Event::from_midi` already uses for events.
    pub(crate) fn route(
        &mut self,
        midi_events: &[MidiEvent],
        param_changes: Option<&ParameterChanges>,
    ) -> Option<(&[MidiEvent], &ParameterChanges)> {
        if self.mapping.is_empty() {
            return None;
        }

        // Clear scratch in place: the param queues go back to the set's
        // spares with their point storage, so the next block reuses them.
        self.filtered_midi.clear();
        self.param_changes.clear();

        // Seed the merged param changes with the host's automation, a
        // queue opened for each of the host's (its addresses are distinct),
        // so the copy is linear in the chunk. The layout fits a floor queue
        // carrying the bridge's carried value in its own room, so the pool's
        // blocks go to the queues at full room wherever they are in the
        // chunk. A queue past the reservation is dropped, counted.
        if let Some(pc) = param_changes {
            let changes = &mut self.param_changes;
            for queue in &pc.queues {
                if !changes.open_queue(queue.param_id) {
                    self.overflowed += queue.points.len() as u64;
                    continue;
                }
                let i = changes.queues.len() - 1;
                for point in &queue.points {
                    let stored = changes.push_point(i, point.sample_offset, point.value.get());
                    self.overflowed += u64::from(!stored);
                }
            }
        }

        route_cc_events(
            &self.mapping,
            midi_events,
            &mut self.filtered_midi,
            &mut self.param_changes,
        );

        // VST3 requires each IParamValueQueue's points in ascending
        // sampleOffset order; seeding + appending can leave them unsorted.
        sort_param_points(&mut self.param_changes);

        Some((&self.filtered_midi, &self.param_changes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::{CCNumber, MidiChannel, MidiGroup};

    #[test]
    fn empty_mapping_returns_none() {
        let m = MidiCcMapping::empty();
        assert!(m.is_empty());
        assert_eq!(m.lookup(0, 1), None);
        assert_eq!(m.lookup(0, CTRL_PITCH_BEND), None);
    }

    #[test]
    fn lookup_resolves_mapped_slots() {
        let mut table = vec![NO_PARAM_ID; NUM_CHANNELS * NUM_CONTROLLERS];
        // Channel 0, mod wheel (CC 1) -> param 100.
        table[1] = 100;
        // Channel 3, pitch bend -> param 200.
        table[3 * NUM_CONTROLLERS + CTRL_PITCH_BEND] = 200;
        let m = MidiCcMapping {
            table,
            any_mapped: true,
        };
        assert!(!m.is_empty());
        assert_eq!(m.lookup(0, 1), Some(100));
        assert_eq!(m.lookup(3, CTRL_PITCH_BEND), Some(200));
        // Unmapped slot on a channel that has other mappings.
        assert_eq!(m.lookup(0, 7), None);
        // Out-of-range channel / controller.
        assert_eq!(m.lookup(16, 1), None);
        assert_eq!(m.lookup(0, NUM_CONTROLLERS), None);
    }

    /// Run a `MidiEvent` through `midi_to_mapped_controller`.
    fn mapped(event: &MidiEvent) -> Option<(u8, usize, f64)> {
        midi_to_mapped_controller(event)
    }

    #[test]
    fn cc_decodes_to_controller_and_normalized_value() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        // CC 74 (brightness) = 64 on channel 2.
        let (ch, ctrl, value) = mapped(&MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::new(2),
            CCNumber::BRIGHTNESS,
            midi1_cc_to_midi2(64),
        ))
        .unwrap();
        assert_eq!(ch, 2);
        assert_eq!(ctrl, 74);
        assert!((value - 64.0 / 127.0).abs() < 0.01);
    }

    #[test]
    fn channel_pressure_decodes_to_aftertouch() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        let (ch, ctrl, value) = mapped(&MidiEvent::channel_pressure(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            midi1_cc_to_midi2(127),
        ))
        .unwrap();
        assert_eq!(ch, 0);
        assert_eq!(ctrl, CTRL_AFTERTOUCH);
        assert!((value - 1.0).abs() < 0.01);
    }

    #[test]
    fn pitch_bend_center_is_half() {
        use tutti_midi_types::convert::midi1_pitch_bend_to_midi2;
        let (ch, ctrl, value) = mapped(&MidiEvent::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(4),
            midi1_pitch_bend_to_midi2(8192),
        ))
        .unwrap();
        assert_eq!(ch, 4);
        assert_eq!(ctrl, CTRL_PITCH_BEND);
        // Center bend maps to the middle of the unit range.
        assert!((value - 0.5).abs() < 0.01);
    }

    #[test]
    fn note_on_is_not_a_mapped_controller() {
        assert_eq!(
            mapped(&MidiEvent::note_on(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                0x8000
            )),
            None
        );
    }

    /// Builds a table with a single channel-0 mod-wheel (CC 1) → param mapping.
    fn mod_wheel_mapping(param_id: ParamID) -> MidiCcMapping {
        let mut table = vec![NO_PARAM_ID; NUM_CHANNELS * NUM_CONTROLLERS];
        table[1] = param_id; // channel 0, CC 1
        MidiCcMapping {
            table,
            any_mapped: true,
        }
    }

    #[test]
    fn route_cc_events_splits_mapped_cc_into_params() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        let mapping = mod_wheel_mapping(500);
        // Mod wheel (mapped) + a note-on (passes through) + an unmapped CC.
        let events = [
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                midi1_cc_to_midi2(64),
            )
            .with_frame_offset(8),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::BRIGHTNESS,
                midi1_cc_to_midi2(100),
            )
            .with_frame_offset(0),
        ];

        let mut filtered = RtMidiEvents::new();
        let mut params = ParameterChanges::reserved(1, 1);
        route_cc_events(&mapping, &events, &mut filtered, &mut params);

        // Mod wheel went to params.
        let queue = params
            .get_queue(ParamAddress::Opaque(500u32.into()))
            .expect("mod wheel mapped to param 500");
        assert_eq!(queue.points.len(), 1);
        assert_eq!(queue.points[0].sample_offset, 8);
        // Value decoded at MIDI-2 width then normalized; ~64/127, not bit-exact
        // (the source CC was a MIDI-1→MIDI-2 promotion).
        assert!((queue.points[0].value.get() - 64.0 / 127.0).abs() < 0.01);

        // Note-on and the unmapped CC 74 stayed as MIDI events.
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|e| e.is_note_on()));
    }

    #[test]
    fn route_cc_events_preserves_host_params() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        let mapping = mod_wheel_mapping(500);
        let events = [MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            CCNumber::MOD_WHEEL,
            midi1_cc_to_midi2(127),
        )
        .with_frame_offset(0)];

        let mut filtered = RtMidiEvents::new();
        let mut params = ParameterChanges::reserved(2, 1);
        // Pre-seed with a host automation point (as the audio path does).
        params.add_change(ParamAddress::Opaque(42u32.into()), 0, 0.25);
        route_cc_events(&mapping, &events, &mut filtered, &mut params);

        assert!(
            params
                .get_queue(ParamAddress::Opaque(42u32.into()))
                .is_some(),
            "host param survived"
        );
        assert!(
            params
                .get_queue(ParamAddress::Opaque(500u32.into()))
                .is_some(),
            "mapped CC added"
        );
        assert!(filtered.is_empty(), "the only event was a mapped CC");
    }

    /// Regression: seeding host automation first, then appending CC-derived
    /// points in MIDI-arrival order, can leave a queue out of sample-offset
    /// order. `sort_param_points` must restore the ascending order VST3
    /// requires — across BOTH sources merged into one param.
    #[test]
    fn sort_param_points_orders_merged_host_and_cc_points() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        let mapping = mod_wheel_mapping(500);
        // Two mapped mod-wheel CCs arriving OUT of frame order (100 before 10).
        let events = [
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                midi1_cc_to_midi2(100),
            )
            .with_frame_offset(100),
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                midi1_cc_to_midi2(10),
            )
            .with_frame_offset(10),
        ];

        let mut filtered = RtMidiEvents::new();
        let mut params = ParameterChanges::reserved(2, 3);
        // Seed the SAME param (500) with host automation at a middle offset,
        // plus a separate param to prove per-queue sorting.
        params.add_change(ParamAddress::Opaque(500u32.into()), 50, 0.5);
        params.add_change(ParamAddress::Opaque(42u32.into()), 30, 0.1);
        params.add_change(ParamAddress::Opaque(42u32.into()), 5, 0.2);
        route_cc_events(&mapping, &events, &mut filtered, &mut params);

        // Before sorting, param 500 is [50 (host), 100 (cc), 10 (cc)] — unsorted.
        sort_param_points(&mut params);

        for param_id in [500, 42] {
            let q = params
                .get_queue(ParamAddress::Opaque(param_id.into()))
                .expect("queue present");
            let offsets: Vec<i32> = q.points.iter().map(|p| p.sample_offset).collect();
            let mut sorted = offsets.clone();
            sorted.sort_unstable();
            assert_eq!(offsets, sorted, "param {param_id} points not ascending");
        }
        // Param 500 carries all three merged points (1 host + 2 CC).
        assert_eq!(
            params
                .get_queue(ParamAddress::Opaque(500u32.into()))
                .unwrap()
                .points
                .len(),
            3
        );
    }

    /// Points sharing a `sample_offset` keep the order they were added in.
    ///
    /// VST3 spells a discontinuity as two points — the old value, then the new
    /// one — and lets them share an offset. Which comes first *is* the jump's
    /// direction, so a sort that reorders equal keys turns a rise into a fall.
    ///
    /// The sibling test above cannot catch this: its offsets are all distinct
    /// (50, 100, 10), and with no equal keys the two sorts are
    /// indistinguishable. It asserts only that the result ascends, which stays
    /// true either way.
    ///
    /// Size and shape here are deliberate, not padding. Rust's unstable sort is
    /// a pattern-defeating quicksort that runs insertion sort on short slices
    /// and detects already-ordered runs, so it happens to preserve order on
    /// small or tidy inputs — measured: a handful of tied points, or ties in
    /// ascending order, come out identical under both sorts at every size
    /// tried. The two diverge from roughly 32 points when the offsets arrive
    /// *descending*, which is what this builds. That is a reachable shape:
    /// `add_point` has no cap, and a dense automation curve merged with CC
    /// traffic on one param exceeds it. A smaller fixture would pass against
    /// either sort and pin nothing.
    #[test]
    fn points_sharing_a_sample_offset_keep_their_insertion_order() {
        let mut params = ParameterChanges::reserved(1, 80);
        let param = ParamAddress::Opaque(500u32.into());

        // 40 offsets arriving high-to-low, each carrying a two-point jump.
        // `value` encodes arrival order so any transposition is visible.
        //
        // The encoding has to stay inside `0..=1`: a point's value is a
        // `Normalized`, which *clamps* rather than rejecting, so a raw counter
        // would land every point on 1.0 and compare equal no matter how the
        // sort transposed them — the assertion would hold against a sort that
        // shuffled ties freely. Dividing by the point count keeps all 80 values
        // distinct and ordered, which is the property being pinned.
        const OFFSETS: i32 = 40;
        const POINTS: f64 = (OFFSETS * 2) as f64;
        let mut expected = Vec::new();
        for i in 0..OFFSETS {
            let offset = (OFFSETS - i) * 4;
            let old = f64::from(i * 2) / POINTS;
            let new = f64::from(i * 2 + 1) / POINTS;
            params.add_change(param, offset, old);
            params.add_change(param, offset, new);
            expected.push((offset, old, new));
        }
        expected.sort_by_key(|(offset, _, _)| *offset);

        sort_param_points(&mut params);

        let q = params.get_queue(param).expect("queue present");
        let got: Vec<(i32, f64)> = q
            .points
            .iter()
            .map(|p| (p.sample_offset, p.value.get()))
            .collect();

        let want: Vec<(i32, f64)> = expected
            .iter()
            .flat_map(|(offset, old, new)| [(*offset, *old), (*offset, *new)])
            .collect();

        assert_eq!(
            got, want,
            "each offset carries a jump old -> new; transposing a tied pair \
             inverts that jump's direction"
        );
    }

    /// A route with no CC mapping reserves no param scratch, and the scratch
    /// is reserved when [`CcRoute::set_mapping`] (what
    /// `rebuild_midi_cc_mapping` calls on the control thread) installs a
    /// non-empty mapping, and released when it installs an empty one. The
    /// sizes are the production cap (16384 queues) and a 4096-frame block's
    /// chunk room, where reserving regardless costs tens of megabytes per
    /// instance. The first block after the rebuild is then routed inside
    /// the no-alloc gate with no warm-up, which the reservation makes
    /// allocation-free.
    ///
    /// Mutations (run): `new` reserving regardless of the mapping (the
    /// previous code) → the empty route's `capacity()` is 16384 → fails;
    /// `set_mapping` not reserving when the mapping becomes non-empty →
    /// `capacity()` stays 0 → fails (and the next test, whose `new` goes
    /// through `set_mapping`, aborts in its gate); `set_mapping` keeping the
    /// scratch when the mapping empties → the last `capacity()` is 16384 →
    /// fails.
    #[test]
    fn cc_route_reserves_only_while_mapped() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        const QUEUES: usize = 1 << 14;
        let room = tutti_plugin_types::ParameterQueue::chunk_room(4096);

        let mut route = CcRoute::new(MidiCcMapping::empty(), QUEUES, room);
        assert_eq!(route.param_changes.capacity(), 0, "no mapping, no queues");
        assert_eq!(route.param_changes.point_storage(), 0, "no spare queues");
        assert!(route.route(&[], None).is_none());

        route.set_mapping(mod_wheel_mapping(500));
        assert_eq!(route.param_changes.capacity(), QUEUES);
        assert_eq!(
            route.param_changes.point_storage(),
            ParameterChanges::point_budget(QUEUES, room)
        );

        let events = [MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            CCNumber::MOD_WHEEL,
            midi1_cc_to_midi2(64),
        )
        .with_frame_offset(3)];
        let mut host = ParameterChanges::reserved(1, 1);
        host.add_change(ParamAddress::Opaque(7u32.into()), 0, 0.25);
        assert_no_alloc::assert_no_alloc(|| {
            let (midi, params) = route.route(&events, Some(&host)).expect("mapped");
            assert!(midi.is_empty(), "the only event was a mapped CC");
            assert_eq!(params.queues.len(), 2, "host automation + the CC");
        });

        route.set_mapping(MidiCcMapping::empty());
        assert_eq!(route.param_changes.capacity(), 0, "released when unmapped");
    }

    /// The very first block is allocation-free with no warm-up: every queue
    /// it opens is a reserved spare with room for `points` points, so the
    /// host automation it seeds (several points on one parameter) and the
    /// CC point it appends push into existing storage. This is what the
    /// `points` argument buys; the warm-up test below cannot see it, because
    /// its warm-up grows the queues before the gate.
    ///
    /// Mutation (run): `CcRoute::new(mapping, 4, 0)` (reserved queues with
    /// no chunk room, only the carried value's point) → the host queue
    /// keeps one point (its latest), not four, and the assertion inside the
    /// gate aborts → fails. (Reworded with
    /// `PointBuf`: the seeded points allocated inside the gate before.)
    #[test]
    fn cc_route_first_block_is_allocation_free_without_warmup() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        let events = [
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                midi1_cc_to_midi2(64),
            )
            .with_frame_offset(16),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
        ];
        let mut host = ParameterChanges::reserved(1, 4);
        for (offset, value) in [(0, 0.1), (8, 0.2), (24, 0.3), (40, 0.4)] {
            host.add_change(ParamAddress::Opaque(7u32.into()), offset, value);
        }

        let mut route = CcRoute::new(mod_wheel_mapping(500), 4, 4);
        assert_no_alloc::assert_no_alloc(|| {
            let (midi, params) = route.route(&events, Some(&host)).expect("mapped");
            assert_eq!(midi.len(), 1, "the note passes through");
            assert_eq!(params.queues.len(), 2, "host automation + the CC");
            let host_queue = params
                .get_queue(ParamAddress::Opaque(7u32.into()))
                .expect("host queue");
            assert_eq!(host_queue.points.len(), 4);
        });
    }

    /// **Merged host automation and CC points never grow the reserved
    /// scratch, past its pool too.** 1 000 parameters at a 512-frame block,
    /// reserved as activation reserves the route. Warmed with one pattern
    /// (the host's chunk moving parameters `0..256` at a full room, one CC
    /// for the mapped parameter), then measured with another inside the
    /// no-alloc gate (parameters `256..512`, so no warm-up left them storage,
    /// and four CCs for the mapped parameter, whose fourth is past its floor
    /// of `FLOOR_POINTS + 1` with the scratch's pool spent): nothing
    /// allocates, the mapped parameter ends on its latest CC value, and one
    /// point is counted. (Reworded with `PointBuf` and the carried layout:
    /// three CCs, past a floor of `FLOOR_POINTS`, before.)
    ///
    /// Mutation (run): `PointBuf::replace_latest` dropping the point (the
    /// queue keeping its earlier points) → the mapped parameter ends on its
    /// third CC's value → fails.
    #[test]
    fn cc_points_past_the_scratch_pool_give_way_without_allocating() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        const QUEUES: usize = 1_000;
        const MAPPED: u32 = 999;
        let room = tutti_plugin_types::ParameterQueue::chunk_room(512);
        let pooled = ParameterChanges::POOLED_QUEUES as u32;
        let chunk = |moving: core::ops::Range<u32>| {
            let mut host = ParameterChanges::reserved(QUEUES, room);
            for id in moving {
                for k in 0..room {
                    assert!(host.add_change(ParamAddress::Opaque(id.into()), k as i32, 0.5));
                }
            }
            host
        };
        let cc = |frame: u32, value: u8| {
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                midi1_cc_to_midi2(value),
            )
            .with_frame_offset(frame)
        };
        let (warm, measured) = (chunk(0..pooled), chunk(pooled..2 * pooled));
        let one = [cc(0, 10)];
        let four = [cc(0, 10), cc(8, 20), cc(16, 30), cc(24, 127)];
        let mut route = CcRoute::new(mod_wheel_mapping(MAPPED), QUEUES, room);
        let _ = route.route(&one, Some(&warm)).expect("mapped");
        assert_eq!(route.points_overflowed(), 0);
        assert_no_alloc::assert_no_alloc(|| {
            let _ = route.route(&four, Some(&measured)).expect("mapped");
        });
        assert_eq!(route.points_overflowed(), 1);
        let (_, params) = route.route(&four, Some(&measured)).expect("mapped");
        let q = params
            .get_queue(ParamAddress::Opaque(MAPPED.into()))
            .expect("the CC");
        assert_eq!(q.points.len(), ParameterChanges::FLOOR_POINTS + 1);
        assert_eq!(
            q.points.last().map(|p| (p.sample_offset, p.value.get())),
            Some((24, 1.0))
        );
        assert_eq!(route.points_overflowed(), 2);
    }

    /// **The host's chunk reaches the route whole, carried values and all,
    /// wherever its full queues are.** 2 000 parameters at a 512-frame
    /// block, the route reserved as activation reserves it. The host's
    /// chunk as the bridge hands it on, moving every parameter but the
    /// mapped one: every floor queue carrying three points (two of its own
    /// and the skipped block's carried value) and the *last*
    /// `POOLED_QUEUES` moved at their room and the carried value. The route
    /// copies it in queue order; nothing is counted, with or without a CC
    /// for the mapped parameter (which takes a spare queue), and nothing
    /// allocates.
    ///
    /// Mutation (run): the seed pushing into each queue's own room only
    /// (`try_push` on the queue, no block borrowed) → the full queues'
    /// points past their floor are counted → fails. (Reworded with the one
    /// layout: the mutation was the scratch in the plain layout, which no
    /// longer exists.)
    #[test]
    fn a_bridged_chunk_reaches_the_route_whole() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        const QUEUES: usize = 2_000;
        const MAPPED: u32 = QUEUES as u32 - 1;
        let moved = QUEUES - 1;
        let room = tutti_plugin_types::ParameterQueue::chunk_room(512);
        let full = moved - ParameterChanges::POOLED_QUEUES..moved;
        let mut host = ParameterChanges::reserved_bridged(QUEUES, room);
        for id in 0..moved {
            let points = if full.contains(&id) {
                room
            } else {
                ParameterChanges::FLOOR_POINTS
            };
            for k in 0..points {
                assert!(host.add_change(
                    ParamAddress::Opaque((id as u32).into()),
                    1 + k as i32,
                    0.5
                ));
            }
            assert!(host.prepend_carried(
                id,
                tutti_plugin_types::ParameterPoint {
                    sample_offset: 0,
                    value: tutti_plugin_types::Normalized::new(0.25),
                },
            ));
        }
        let mapped = MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            CCNumber::MOD_WHEEL,
            midi1_cc_to_midi2(64),
        );
        for events in [&[][..], &[mapped][..]] {
            let mut route = CcRoute::new(mod_wheel_mapping(MAPPED), QUEUES, room);
            assert_no_alloc::assert_no_alloc(|| {
                let _ = route.route(events, Some(&host)).expect("mapped");
            });
            assert_eq!(route.points_overflowed(), 0, "{} CC events", events.len());
            let (_, params) = route.route(events, Some(&host)).expect("mapped");
            for (got, want) in params.queues.iter().zip(&host.queues) {
                assert_eq!(got.param_id, want.param_id);
                assert_eq!(got.points.len(), want.points.len());
            }
            let cc = params.get_queue(ParamAddress::Opaque(MAPPED.into()));
            assert_eq!(
                cc.map(|q| q.points.len()),
                (!events.is_empty()).then_some(1)
            );
        }
    }

    /// **The seed copies the host's chunk queue by queue, and counts what
    /// it cannot hold.** A route reserved for two queues of a room of two
    /// (three points each), seeded inside the no-alloc gate from a chunk of
    /// three queues: the first two are copied, the first's fourth point
    /// keeping its latest value (one counted), and the third queue, past
    /// the reservation, is dropped with its two points counted.
    ///
    /// Mutations (run), each fails: the seed not counting a point past a
    /// queue's room → the count is 2; not counting a queue past the
    /// reservation → the count is 1. Not covered: the seed's cost, linear in
    /// the chunk (through `add_change`, as it was, it scanned the open
    /// queues for every point); nothing but time observes it.
    #[test]
    fn the_seed_copies_the_host_chunk_and_counts_what_it_cannot_hold() {
        let mut host = ParameterChanges::reserved(3, 4);
        for (id, points) in [(1u32, 4), (2, 3), (3, 2)] {
            for k in 0..points {
                assert!(host.add_change(ParamAddress::Opaque(id.into()), 10 * k, 0.5));
            }
        }
        let mut route = CcRoute::new(mod_wheel_mapping(500), 2, 2);
        assert_no_alloc::assert_no_alloc(|| {
            let _ = route.route(&[], Some(&host)).expect("mapped");
        });
        let got: Vec<(u32, Vec<i32>)> = route
            .param_changes
            .queues
            .iter()
            .map(|q| {
                let id = q.param_id.opaque().map_or(0, |id| id.get());
                (id, q.points.iter().map(|p| p.sample_offset).collect())
            })
            .collect();
        assert_eq!(got, [(1, vec![0, 10, 30]), (2, vec![0, 10, 20])]);
        assert_eq!(route.points_overflowed(), 3);
    }

    /// **A queue with two sources ends on its latest value.** The host's
    /// chunk gives a parameter a full queue, its last point late in the
    /// block; a mapped CC for it arrives earlier, with the scratch's room
    /// spent. The CC is earlier than the host's latest point, so the host's
    /// point stays and the queue ends on it; a CC later than every host
    /// point takes the latest's place and the queue ends on the CC.
    ///
    /// Mutation (run): the merge replacing the queue's last point whatever
    /// the new point's time (`PointBuf::replace_latest` without its `>=`
    /// check, as the `pop` and push did) → the early CC evicts the host's
    /// frame-400 point and the sorted queue ends on frame 300 → fails. (The
    /// last point *added* rather than the latest in time, `.last()` in
    /// `replace_latest`, survives here, since the host's points arrive in
    /// order; `push_or_replace_last_keeps_the_latest_value` kills it.)
    #[test]
    fn a_queue_with_host_and_cc_points_ends_on_its_latest_value() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        const MAPPED: u32 = 7;
        // One queue, at the full carried room (`room + 1`), the host's chunk
        // filling it.
        let room = 4;
        let mut host = ParameterChanges::reserved(1, room + 1);
        for (offset, value) in [(0, 0.0), (100, 0.25), (200, 0.5), (300, 0.625), (400, 0.75)] {
            assert!(host.add_change(ParamAddress::Opaque(MAPPED.into()), offset, value));
        }
        let cc = |frame: u32, value: u8| {
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                midi1_cc_to_midi2(value),
            )
            .with_frame_offset(frame)
        };
        let mut route = CcRoute::new(mod_wheel_mapping(MAPPED), 1, room);
        let mut ends = |events: &[MidiEvent]| {
            let (_, params) = route.route(events, Some(&host)).expect("mapped");
            let q = params
                .get_queue(ParamAddress::Opaque(MAPPED.into()))
                .expect("queued");
            assert!(q
                .points
                .windows(2)
                .all(|w| w[0].sample_offset <= w[1].sample_offset));
            q.points.last().map(|p| (p.sample_offset, p.value.get()))
        };
        assert_eq!(ends(&[cc(150, 127)]), Some((400, 0.75)));
        assert_eq!(ends(&[cc(450, 127)]), Some((450, 1.0)));
        assert_eq!(route.points_overflowed(), 2);
    }

    /// **The merge sorts a queue at a chunk's full room without
    /// allocating, stably.** One parameter at a 4096-frame block's carried
    /// room (`chunk_room(4096) + 1` = 386 points): the host's chunk fills
    /// all but eight with points every 8 frames, and eight mapped CCs land
    /// on frames among them (four on a host point's frame), so the merged
    /// queue is out of order and must be sorted, inside the no-alloc gate.
    /// The result is in time order, holds every point, and a CC that shares
    /// a frame with a host point comes after it (the host's points are
    /// seeded first).
    ///
    /// Mutation (run): `sort_param_points` back to `queue.points
    /// .sort_by_key(|point| point.sample_offset)` → std's stable sort
    /// allocates its scratch for 386 points → the gate aborts. (Before the
    /// raised room this could not be seen: a queue held at most 133 points,
    /// under std's 256-point stack buffer.)
    #[test]
    fn the_merge_sorts_a_full_room_queue_without_allocating() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        const MAPPED: u32 = 7;
        const CCS: usize = 8;
        let room = tutti_plugin_types::ParameterQueue::chunk_room(4096);
        let full = room + 1;
        assert_eq!(full, 386);
        let mut host = ParameterChanges::reserved(1, room);
        for k in 0..full - CCS {
            assert!(host.add_change(ParamAddress::Opaque(MAPPED.into()), 8 * k as i32, 0.25));
        }
        let cc = |frame: u32, value: u8| {
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                midi1_cc_to_midi2(value),
            )
            .with_frame_offset(frame)
        };
        // On host frames (multiples of 8) and between them.
        let events: Vec<MidiEvent> = (0..CCS as u32)
            .map(|k| cc(100 * k + if k % 2 == 0 { 0 } else { 3 }, 127))
            .collect();
        let mut route = CcRoute::new(mod_wheel_mapping(MAPPED), 1, room);
        let sorted = assert_no_alloc::assert_no_alloc(|| {
            let (_, params) = route.route(&events, Some(&host)).expect("mapped");
            let q = params
                .get_queue(ParamAddress::Opaque(MAPPED.into()))
                .expect("queued");
            q.points
                .iter()
                .map(|p| (p.sample_offset, p.value.get()))
                .fold((0usize, true, i32::MIN), |(n, ok, last), (o, _)| {
                    (n + 1, ok && last <= o, o)
                })
        });
        assert_eq!(sorted, (full, true, 8 * (full - CCS - 1) as i32));
        assert_eq!(route.points_overflowed(), 0);
        let (_, params) = route.route(&events, Some(&host)).expect("mapped");
        let q = params
            .get_queue(ParamAddress::Opaque(MAPPED.into()))
            .expect("queued");
        // A shared frame: the host's point, then the CC's.
        for k in (0..CCS as i32).step_by(2) {
            let at: Vec<f64> = q
                .points
                .iter()
                .filter(|p| p.sample_offset == 100 * k)
                .map(|p| p.value.get())
                .collect();
            assert_eq!(at, [0.25, 1.0], "frame {}", 100 * k);
        }
    }

    /// RT regression: once the scratch buffers are warmed, repeated routing
    /// must not allocate. Drives [`CcRoute::route`] itself, host automation
    /// seeded and all, rather than a copy of its clear discipline: the
    /// discipline is what changed when the queues stopped holding points
    /// inline (a queue dropped by `queues.clear()` and rebuilt has no room,
    /// so its first point allocates), and a copy would not have followed it.
    ///
    /// Mutation (run): `route` clearing with `queues.clear()` (dropping the
    /// queues, the old discipline) instead of `ParameterChanges::clear` →
    /// every block rebuilds its queues inside the gate → fails.
    #[test]
    fn route_cc_events_is_allocation_free_after_warmup() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        let mapping = mod_wheel_mapping(500);
        let events = [
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::MOD_WHEEL,
                midi1_cc_to_midi2(64),
            )
            .with_frame_offset(0),
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
                .with_frame_offset(0),
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0).with_frame_offset(64),
            MidiEvent::cc(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                CCNumber::BRIGHTNESS,
                midi1_cc_to_midi2(10),
            )
            .with_frame_offset(32),
        ];

        let mut route = CcRoute::new(mapping, 4, 16);
        let mut host = ParameterChanges::reserved(1, 2);
        host.add_change(ParamAddress::Opaque(7u32.into()), 0, 0.25);
        host.add_change(ParamAddress::Opaque(7u32.into()), 48, 0.75);

        // Warm up once, outside the gate.
        let _ = route.route(&events, Some(&host));

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..10_000 {
                let (midi, params) = route.route(&events, Some(&host)).expect("mapped");
                assert_eq!(midi.len(), 3, "the mapped CC is taken out");
                assert_eq!(params.queues.len(), 2, "host automation + the CC");
            }
        });
    }
}
