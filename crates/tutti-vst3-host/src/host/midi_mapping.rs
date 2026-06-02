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

/// A MIDI-1 message decomposed into the `(controller, normalized_value)` a
/// VST3 `IMidiMapping` parameter expects, or `None` if the message is not a
/// mappable controller (CC / channel pressure / pitch bend).
///
/// Value normalization matches the VST3 host contract:
/// - CC / channel pressure: 7-bit `0..=127` → `0.0..=1.0`.
/// - Pitch bend: 14-bit `0..=16383` → `0.0..=1.0`, with `8192` (center) at
///   `0.5`.
pub(crate) fn midi1_to_mapped_controller(status: u8, d1: u8, d2: u8) -> Option<(usize, f64)> {
    match status & 0xF0 {
        // Control change: controller number = d1, value = d2.
        0xB0 => Some((d1 as usize, d2 as f64 / 127.0)),
        // Channel pressure (channel aftertouch): value = d1.
        0xD0 => Some((CTRL_AFTERTOUCH, d1 as f64 / 127.0)),
        // Pitch bend: 14-bit little-endian (LSB=d1, MSB=d2).
        0xE0 => {
            let bend14 = (d1 as u16 & 0x7F) | ((d2 as u16 & 0x7F) << 7);
            Some((CTRL_PITCH_BEND, bend14 as f64 / 16383.0))
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
/// this routine only appends. Decoding uses the lossless MIDI-1 byte form.
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
        let Some((bytes, _len)) = event.to_midi1_bytes() else {
            out_filtered.push(*event);
            continue;
        };
        let status = bytes[0];
        let channel = status & 0x0F;
        match midi1_to_mapped_controller(status, bytes[1], bytes[2]) {
            Some((controller, value)) => match mapping.lookup(channel, controller) {
                Some(param_id) => {
                    out_params.add_change(param_id, event.frame_offset as i32, value);
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

    #[test]
    fn cc_decodes_to_controller_and_normalized_value() {
        // CC 74 (brightness) = 64 on channel 2.
        let (ctrl, value) = midi1_to_mapped_controller(0xB2, 74, 64).unwrap();
        assert_eq!(ctrl, 74);
        assert!((value - 64.0 / 127.0).abs() < 1e-9);
    }

    #[test]
    fn channel_pressure_decodes_to_aftertouch() {
        let (ctrl, value) = midi1_to_mapped_controller(0xD0, 127, 0).unwrap();
        assert_eq!(ctrl, CTRL_AFTERTOUCH);
        assert!((value - 1.0).abs() < 1e-9);
    }

    #[test]
    fn pitch_bend_center_is_half() {
        // 8192 = LSB 0x00, MSB 0x40.
        let (ctrl, value) = midi1_to_mapped_controller(0xE0, 0x00, 0x40).unwrap();
        assert_eq!(ctrl, CTRL_PITCH_BEND);
        assert!((value - 8192.0 / 16383.0).abs() < 1e-9);
        assert!((value - 0.5).abs() < 0.01);
    }

    #[test]
    fn note_on_is_not_a_mapped_controller() {
        assert_eq!(midi1_to_mapped_controller(0x90, 60, 100), None);
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
        let queue = params.get_queue(500).expect("mod wheel mapped to param 500");
        assert_eq!(queue.points.len(), 1);
        assert_eq!(queue.points[0].sample_offset, 8);
        assert!((queue.points[0].value - 64.0 / 127.0).abs() < 1e-9);

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
        params.add_change(42, 0, 0.25);
        route_cc_events(&mapping, &events, &mut filtered, &mut params);

        assert!(params.get_queue(42).is_some(), "host param survived");
        assert!(params.get_queue(500).is_some(), "mapped CC added");
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
        params.add_change(500, 50, 0.5);
        params.add_change(42, 30, 0.1);
        params.add_change(42, 5, 0.2);
        route_cc_events(&mapping, &events, &mut filtered, &mut params);

        // Before sorting, param 500 is [50 (host), 100 (cc), 10 (cc)] — unsorted.
        sort_param_points(&mut params);

        for param_id in [500, 42] {
            let q = params.get_queue(param_id).expect("queue present");
            let offsets: Vec<i32> = q.points.iter().map(|p| p.sample_offset).collect();
            let mut sorted = offsets.clone();
            sorted.sort_unstable();
            assert_eq!(offsets, sorted, "param {param_id} points not ascending");
        }
        // Param 500 carries all three merged points (1 host + 2 CC).
        assert_eq!(params.get_queue(500).unwrap().points.len(), 3);
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
