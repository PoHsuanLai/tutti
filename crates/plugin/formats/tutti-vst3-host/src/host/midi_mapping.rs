//! VST3 `IMidiMapping` support: the CC→parameter routing table.
//!
//! Many VST3 instruments expose mod-wheel, breath, expression, sustain, etc.
//! *only* through `IMidiMapping::getMidiControllerAssignment` — the spec's
//! intended path — rather than reacting to raw CC `Data` events. Without this
//! query those controllers silently do nothing.
//!
//! At load (UI thread) we walk every channel × controller-number slot and ask
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

use smallvec::SmallVec;
use vst3::ComPtr;
use vst3::Steinberg::kResultTrue;
use vst3::Steinberg::Vst::{
    ControllerNumbers_::{kAfterTouch, kCountCtrlNumber, kPitchBend},
    IEditController, IMidiMapping, IMidiMappingTrait, ParamID,
};

use crate::types::{MidiEvent, ParameterChanges};
use tutti_plugin_types::ParamAddress;

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

    /// Query the controller's `IMidiMapping` (if it exposes one) for every
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

    /// Look up the `ParamID` a given channel/controller drives, or `None` if
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

/// Split `midi_events` against `mapping`: mapped CC/aftertouch/pitch-bend
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
    out_filtered: &mut SmallVec<[MidiEvent; 64]>,
    out_params: &mut ParameterChanges,
) {
    for event in midi_events {
        let mapped = midi_to_mapped_controller(event);
        match mapped {
            Some((channel, controller, value)) => match mapping.lookup(channel, controller) {
                Some(param_id) => {
                    // `IMidiMapping` answers with a `ParamID` — opaque, like
                    // every VST3 parameter address.
                    out_params.add_change(
                        ParamAddress::Opaque(param_id.into()),
                        event.frame_offset as i32,
                        value,
                    );
                }
                // Mappable message but no mapping for this slot — keep it as a
                // MIDI event so the plugin can still react.
                None => out_filtered.push(*event),
            },
            // Not a mappable controller (note on/off, program change, …).
            None => out_filtered.push(*event),
        }
    }
}

/// Sort every queue's points into ascending `sample_offset` order, as VST3's
/// `IParamValueQueue` requires. Host automation is seeded before CC-derived
/// points are appended (in MIDI-arrival order), so a queue carrying both can be
/// unsorted even when each source was individually ordered. In place — the
/// `points` SmallVec reuses its capacity, so this is allocation-free.
pub(crate) fn sort_param_points(params: &mut ParameterChanges) {
    for queue in params.queues.iter_mut() {
        queue
            .points
            .sort_unstable_by_key(|point| point.sample_offset);
    }
}

/// The `IMidiMapping` CC→param routing concern as a per-block unit: the
/// controller-queried mapping table plus the pooled scratch the routing pass
/// needs. Held by `Vst3Instance`'s `AudioIO`.
///
/// When `mapping` is non-empty, mapped CC/aftertouch/pitch-bend events are
/// pulled out of the input MIDI into `filtered_midi` (the events that still
/// reach the plugin's event list) while their parameter points are merged into
/// `param_changes` alongside the host's automation. The scratch buffers are
/// reused across blocks to keep the routing pass allocation-free; `mapping` is
/// rebuilt only when the plugin signals `kMidiCCAssignmentChanged` (see
/// [`Vst3Instance::rebuild_midi_cc_mapping`](crate::Vst3Instance)).
pub(crate) struct CcRoute {
    pub(crate) mapping: MidiCcMapping,
    filtered_midi: SmallVec<[MidiEvent; 64]>,
    param_changes: ParameterChanges,
}

impl CcRoute {
    /// Build with an empty mapping queried from `controller`, plus empty
    /// scratch. The scratch grows once on first use and is reused thereafter.
    pub(crate) fn new(mapping: MidiCcMapping) -> Self {
        Self {
            mapping,
            filtered_midi: SmallVec::new(),
            param_changes: ParameterChanges::new(),
        }
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

        // Clear scratch in place (keep heap capacity).
        self.filtered_midi.clear();
        for queue in self.param_changes.queues.iter_mut() {
            queue.points.clear();
        }
        self.param_changes.queues.clear();

        // Seed the merged param changes with the host's automation.
        if let Some(pc) = param_changes {
            for queue in &pc.queues {
                for point in &queue.points {
                    self.param_changes
                        .add_change(queue.param_id, point.sample_offset, point.value);
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
        let (ch, ctrl, value) = mapped(&MidiEvent::cc(0, 2, 74, midi1_cc_to_midi2(64))).unwrap();
        assert_eq!(ch, 2);
        assert_eq!(ctrl, 74);
        assert!((value - 64.0 / 127.0).abs() < 0.01);
    }

    #[test]
    fn channel_pressure_decodes_to_aftertouch() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        let (ch, ctrl, value) =
            mapped(&MidiEvent::channel_pressure(0, 0, midi1_cc_to_midi2(127))).unwrap();
        assert_eq!(ch, 0);
        assert_eq!(ctrl, CTRL_AFTERTOUCH);
        assert!((value - 1.0).abs() < 0.01);
    }

    #[test]
    fn pitch_bend_center_is_half() {
        use tutti_midi_types::convert::midi1_pitch_bend_to_midi2;
        let (ch, ctrl, value) = mapped(&MidiEvent::pitch_bend(
            0,
            4,
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
        assert_eq!(mapped(&MidiEvent::note_on(0, 0, 60, 0x8000)), None);
    }

    /// Build a table with a single channel-0 mod-wheel (CC 1) → param mapping.
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
            MidiEvent::cc(0, 0, 1, midi1_cc_to_midi2(64)).with_frame_offset(8),
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::cc(0, 0, 74, midi1_cc_to_midi2(100)).with_frame_offset(0),
        ];

        let mut filtered = SmallVec::new();
        let mut params = ParameterChanges::new();
        route_cc_events(&mapping, &events, &mut filtered, &mut params);

        // Mod wheel went to params.
        let queue = params
            .get_queue(ParamAddress::Opaque(500u32.into()))
            .expect("mod wheel mapped to param 500");
        assert_eq!(queue.points.len(), 1);
        assert_eq!(queue.points[0].sample_offset, 8);
        // Value decoded at MIDI-2 width then normalized; ~64/127, not bit-exact
        // (the source CC was a MIDI-1→MIDI-2 promotion).
        assert!((queue.points[0].value - 64.0 / 127.0).abs() < 0.01);

        // Note-on and the unmapped CC 74 stayed as MIDI events.
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|e| e.is_note_on()));
    }

    #[test]
    fn route_cc_events_preserves_host_params() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        let mapping = mod_wheel_mapping(500);
        let events = [MidiEvent::cc(0, 0, 1, midi1_cc_to_midi2(127)).with_frame_offset(0)];

        let mut filtered = SmallVec::new();
        let mut params = ParameterChanges::new();
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
            MidiEvent::cc(0, 0, 1, midi1_cc_to_midi2(100)).with_frame_offset(100),
            MidiEvent::cc(0, 0, 1, midi1_cc_to_midi2(10)).with_frame_offset(10),
        ];

        let mut filtered = SmallVec::new();
        let mut params = ParameterChanges::new();
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

    /// RT regression: once the scratch buffers are warmed, repeated routing
    /// must not allocate. Mirrors the audio path's clear-in-place discipline.
    #[test]
    fn route_cc_events_is_allocation_free_after_warmup() {
        use tutti_midi_types::convert::midi1_cc_to_midi2;
        let mapping = mod_wheel_mapping(500);
        let events = [
            MidiEvent::cc(0, 0, 1, midi1_cc_to_midi2(64)).with_frame_offset(0),
            MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(0),
            MidiEvent::note_off(0, 0, 60, 0).with_frame_offset(64),
            MidiEvent::cc(0, 0, 74, midi1_cc_to_midi2(10)).with_frame_offset(32),
        ];

        let mut filtered: SmallVec<[MidiEvent; 64]> = SmallVec::new();
        let mut params = ParameterChanges::new();

        // Warm up: grow both buffers' capacity once.
        let clear = |filtered: &mut SmallVec<[MidiEvent; 64]>, params: &mut ParameterChanges| {
            filtered.clear();
            for q in params.queues.iter_mut() {
                q.points.clear();
            }
            params.queues.clear();
        };
        route_cc_events(&mapping, &events, &mut filtered, &mut params);
        clear(&mut filtered, &mut params);

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..10_000 {
                route_cc_events(&mapping, &events, &mut filtered, &mut params);
                clear(&mut filtered, &mut params);
            }
        });
    }
}
