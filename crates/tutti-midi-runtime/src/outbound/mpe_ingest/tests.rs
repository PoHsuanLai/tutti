use super::*;
use tutti_fixtures::midi;
use tutti_graph::UmpAt;
use tutti_midi_types::MidiMessage;
use tutti_midi_types::mpe::MpeZoneConfig;
use tutti_types::Samples;
use tutti_ump::{Address, CCNumber, Message, Voice2};

/// What the ingest sends, collected: the tests read it as a list.
trait Collected {
    fn translated(&mut self, event: &UmpAt) -> std::vec::IntoIter<UmpAt>;
    fn defaults(&self, group: MidiGroup, offset: Offset) -> std::vec::IntoIter<UmpAt>;
}

impl Collected for MpeIngest {
    fn translated(&mut self, event: &UmpAt) -> std::vec::IntoIter<UmpAt> {
        let mut out = Vec::new();
        self.translate(event, |e| out.push(e));
        out.into_iter()
    }

    fn defaults(&self, group: MidiGroup, offset: Offset) -> std::vec::IntoIter<UmpAt> {
        let mut out = Vec::new();
        self.zone_defaults(group, offset, |e| out.push(e));
        out.into_iter()
    }
}

/// `ump` at frame `offset` of a 64-frame block.
fn at(offset: usize, ump: Ump) -> UmpAt {
    (Offset::new(offset, Samples(64)).unwrap(), ump)
}

/// The channel nibble of a channel voice packet.
fn channel_of(ump: &Ump) -> u8 {
    u8::from(ump.channel().expect("a channel voice packet"))
}

fn note_on(channel: u8, note: u8, vel_u7: u8) -> UmpAt {
    at(
        0,
        midi::note_on(
            MidiGroup::FIRST,
            MidiChannel::of(channel),
            note,
            tutti_fixtures::midi::wide_velocity(vel_u7),
        ),
    )
}
fn pitch_bend14(channel: u8, bend14: u16) -> UmpAt {
    at(
        0,
        midi::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::of(channel),
            tutti_fixtures::midi::wide_bend(bend14),
        ),
    )
}
fn cc(channel: u8, cc_num: CCNumber, value_u7: u8) -> UmpAt {
    at(
        0,
        midi::cc(
            MidiGroup::FIRST,
            MidiChannel::of(channel),
            cc_num,
            tutti_fixtures::midi::wide_value(value_u7),
        ),
    )
}

/// True if the event decodes to the given MIDI 2.0 channel voice message:
/// `$addr` binds its `Address`, `$pat` matches the `Voice2`.
macro_rules! assert_cv2 {
    ($ev:expr, $addr:pat, $pat:pat $(if $guard:expr)?) => {
        match tutti_ump::decode($ev.1) {
            Ok(Message::Midi2($addr, $pat)) $(if $guard)? => {}
            other => panic!("unexpected message: {other:?}"),
        }
    };
}

#[test]
fn member_channel_bend_becomes_per_note_pitch_bend() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    // Note held on member channel 2.
    assert!(
        ingest
            .translated(&note_on(2, 60, 100))
            .next()
            .map(|(_, ump)| MidiMessage::from_ump(ump).is_note_on())
            .unwrap()
    );
    // Channel bend on ch2 → per-note bend on note 60.
    let out = ingest
        .translated(&pitch_bend14(2, 16383))
        .next()
        .expect("emits");
    assert_cv2!(
        out,
        Address { channel, .. },
        Voice2::PerNotePitchBend { note, .. }
            if channel.get() == 2 && note.get() == 60
    );
}

#[test]
fn master_channel_bend_passes_through_as_channel_bend() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    // Master channel of a lower zone is channel 0.
    let out = ingest
        .translated(&pitch_bend14(0, 12288))
        .next()
        .expect("passes through");
    assert_cv2!(out, _, Voice2::PitchBend(_));
}

#[test]
fn member_channel_cc74_becomes_per_note_controller() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    ingest.translated(&note_on(3, 64, 100));
    let out = ingest
        .translated(&cc(3, CCNumber::BRIGHTNESS, 127))
        .next()
        .expect("emits");
    assert_cv2!(
        out,
        _,
        Voice2::AssignablePerNote { note, controller, .. }
            if note.get() == 64 && controller == 74
    );
}

#[test]
fn member_channel_pressure_becomes_per_note_pressure() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    ingest.translated(&note_on(2, 60, 100));
    let out = ingest
        .translated(&at(
            0,
            midi::channel_pressure(MidiGroup::FIRST, MidiChannel::of(2), 0xFFFF_FFFF),
        ))
        .next()
        .expect("emits");
    assert_cv2!(out, _, Voice2::PolyPressure { note, .. } if note.get() == 60);
}

#[test]
fn bend_with_no_held_note_is_dropped() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    // No note on ch2 yet → member-channel bend has nothing to address.
    assert!(ingest.translated(&pitch_bend14(2, 16383)).next().is_none());
}

#[test]
fn native_per_note_passes_through() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    let native = at(
        0,
        midi::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF_FFFF),
    );
    let out = ingest.translated(&native).next().expect("passes");
    assert_cv2!(out, _, Voice2::PerNotePitchBend { .. });
}

#[test]
fn disabled_mode_passes_everything_through() {
    let mut ingest = MpeIngest::new(MpeMode::Disabled);
    let out = ingest
        .translated(&pitch_bend14(2, 16383))
        .next()
        .expect("passes");
    assert_cv2!(out, _, Voice2::PitchBend(_));
}

#[test]
fn rotation_ignores_off_channel() {
    let mut ingest = MpeIngest::new(MpeMode::SingleChannelRotation { channel: 5 });
    // On-channel note passes.
    assert!(ingest.translated(&note_on(5, 60, 100)).next().is_some());
    // Off-channel note is ignored (this mode owns channel 5).
    assert!(ingest.translated(&note_on(7, 64, 100)).next().is_none());
}

#[test]
fn mcm_from_the_wire_configures_the_lower_zone() {
    // Start disabled; an MCM on the master channel (0) declaring 7 members
    // must enable the lower zone from the wire (M2-104 §7.4.7).
    let mut ingest = MpeIngest::new(MpeMode::Disabled);
    let mcm = at(0, MpeZoneConfig::lower(7).to_mcm());

    // The MCM itself is absorbed (configuration, not a musical event);
    // what goes on is the zone's reset ranges, all registered controllers
    // (`an_mcm_resets_its_zones_bend_ranges`).
    let reset: Vec<UmpAt> = ingest.translated(&mcm).collect();
    assert_eq!(reset.len(), 8, "master and seven members");
    assert!(!reset.contains(&mcm));
    assert!(matches!(ingest.mode(), MpeMode::LowerZone(_)));

    // Now a member-channel bend folds to per-note, proving the zone is live.
    ingest.translated(&note_on(1, 60, 100));
    let out = ingest
        .translated(&pitch_bend14(1, 16383))
        .next()
        .expect("emits");
    assert_cv2!(out, _, Voice2::PerNotePitchBend { note, .. } if note.get() == 60);
}

#[test]
fn mcm_with_zero_members_disables_the_zone() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(7)));
    // A raw MCM on master channel 0 with member_count == 0 disables the lower
    // zone. (`MpeZoneConfig::lower` clamps to >= 1, so the disabling value can
    // only come off the wire — build it directly.)
    let disable = at(
        0,
        midi::registered_rpn(
            MidiGroup::FIRST,
            MidiChannel::FIRST, // lower-zone master channel
            Rpn::MPE_CONFIGURATION,
            0, // 0 members → disable
        ),
    );
    assert!(ingest.translated(&disable).next().is_none());
    assert!(matches!(ingest.mode(), MpeMode::Disabled));
}

/// What a receiver following RPN #00/00 (channel bend range) and
/// #00/07 (per-note bend range) per channel ends with after `events`:
/// `(channel, index) → semitones`, in order of arrival.
fn ranges_after(events: impl IntoIterator<Item = UmpAt>) -> Vec<(u8, u8, f32)> {
    events
        .into_iter()
        .map(|ev| {
            let Ok(Message::Midi2(address, Voice2::Registered { bank, index, value })) =
                tutti_ump::decode(ev.1)
            else {
                panic!("{ev:?} is not a registered controller");
            };
            assert_eq!(bank.get(), Rpn::PITCH_BEND_SENSITIVITY.bank().get());
            let range = PitchBendSensitivity::from_rpn_bits(value);
            (address.channel.get(), index.get(), range.as_semitones_f32())
        })
        .collect()
}

/// **An MPE Configuration Message resets its zone's bend ranges** (MPE,
/// RP-053 / M1-100-UM, "Pitch Bend Sensitivity": the master channel to
/// ±2, every member to ±48): an upper-zone MCM of three members yields
/// RPN #00/00 = 2 on channel 15 and RPN #00/07 = 48 on 12, 13 and 14, at
/// the message's offset, and a per-note receiver that had been told ±12
/// on those members is back at ±48. The other zone of a dual setup keeps
/// its ranges (nothing is sent for it), and a disabling MCM sends nothing.
/// A host-set mode sends the same through `zone_defaults`, for each zone.
///
/// Mutation (run): the MCM arm returns `None` again (absorbed, nothing
/// sent) → the receiver keeps ±12 → fails.
/// Mutation (run): `zone_default_runs` emits #00/00 on the members → the
/// receiver keeps ±12 → fails.
/// Mutation (run): the MCM resets every zone of the new mode
/// (`zone_defaults` in place of the configured side's runs) → the lower
/// zone's ranges are sent too → fails.
#[test]
fn an_mcm_resets_its_zones_bend_ranges() {
    use tutti_plugin_types::PerNoteBendRanges;
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(3)));
    let mut receiver = PerNoteBendRanges::default();
    for ev in ingest.translated(&bend_sensitivity(1, 12)) {
        receiver.observe(&ev.1);
    }
    let mcm = at(9, MpeZoneConfig::upper(3).to_mcm());
    let reset: Vec<UmpAt> = ingest.translated(&mcm).collect();
    assert!(matches!(ingest.mode(), MpeMode::DualZone { .. }));
    assert!(reset.iter().all(|ev| ev.0.get() == 9));
    assert_eq!(
        ranges_after(reset.iter().copied()),
        vec![(15, 0, 2.0), (12, 7, 48.0), (13, 7, 48.0), (14, 7, 48.0)]
    );
    // An upper member set to ±12, then the upper MCM again: the upper
    // members are back at ±48, the lower zone's ±12 stands.
    let mcm = at(0, MpeZoneConfig::upper(3).to_mcm());
    for ev in ingest.translated(&bend_sensitivity(13, 12)) {
        receiver.observe(&ev.1);
    }
    assert_eq!(receiver.semitones(13), 12.0);
    for ev in ingest.translated(&mcm) {
        receiver.observe(&ev.1);
    }
    for channel in 12..=14 {
        assert_eq!(receiver.semitones(channel), 48.0, "channel {channel}");
    }
    assert_eq!(
        receiver.semitones(1),
        12.0,
        "the lower zone keeps its range"
    );

    let disable = at(
        0,
        midi::registered_rpn(
            MidiGroup::FIRST,
            MidiChannel::of(15),
            Rpn::MPE_CONFIGURATION,
            0,
        ),
    );
    assert_eq!(ingest.translated(&disable).len(), 0);

    // A host-set dual-zone mode: each zone's ranges, lower first.
    ingest.set_mode(MpeMode::DualZone {
        lower: MpeZoneConfig::lower(2),
        upper: MpeZoneConfig::upper(1),
    });
    let defaults = ingest.defaults(MidiGroup::FIRST, Offset::ZERO);
    assert_eq!(defaults.len(), 5);
    assert_eq!(
        ranges_after(defaults),
        vec![
            (0, 0, 2.0),
            (1, 7, 48.0),
            (2, 7, 48.0),
            (15, 0, 2.0),
            (14, 7, 48.0)
        ]
    );
    ingest.set_mode(MpeMode::Disabled);
    assert_eq!(ingest.defaults(MidiGroup::FIRST, Offset::ZERO).len(), 0);
}

fn bend_sensitivity(channel: u8, semitones: u8) -> UmpAt {
    at(
        17,
        midi::registered_rpn(
            MidiGroup::FIRST,
            MidiChannel::of(channel),
            Rpn::PITCH_BEND_SENSITIVITY,
            PitchBendSensitivity::of(semitones).to_rpn_bits(),
        ),
    )
}

/// **A member channel's bend range is every member's per-note range; the
/// master's is its own.** Dual zones of three members each (lower 1..=3,
/// upper 12..=14): RPN #00/00 on member 2 becomes RPN #00/07 on 1, 2 and
/// 3, on member 13 on 12, 13 and 14, at the event's offset; on master 0 it
/// passes through as it came. A per-note receiver reading the output then
/// bends those channels' notes by the new range and the rest by ±48.
///
/// Mutation (run): the fan-out drops its `is_member` check → the master's
/// sensitivity fans out too → fails.
/// Mutation (run): the fan-out emits index #00/00 → the receiver keeps
/// ±48 → fails.
/// Mutation (run): the fan-out takes the lower zone's config for both
/// sides → member 13's fans out over 1..=3 → fails.
/// Mutation (run): one channel (`ch..=ch`) instead of the zone's range →
/// fails.
#[test]
fn a_member_channels_bend_range_becomes_every_members_per_note_range() {
    use tutti_plugin_types::PerNoteBendRanges;
    let mut ingest = MpeIngest::new(MpeMode::DualZone {
        lower: MpeZoneConfig::lower(3),
        upper: MpeZoneConfig::upper(3),
    });
    let mut receiver = PerNoteBendRanges::default();

    let master = bend_sensitivity(0, 2);
    let out: Vec<UmpAt> = ingest.translated(&master).collect();
    assert_eq!(out, vec![master], "the master's own range passes through");
    assert!(!receiver.observe(&out[0].1));

    for (member, semitones, channels) in [(2, 12, 1..=3), (13, 24, 12..=14)] {
        let fanned = ingest.translated(&bend_sensitivity(member, semitones));
        assert_eq!(fanned.len(), 3);
        let mut seen = Vec::new();
        for ev in fanned {
            assert_eq!(ev.0.get(), 17);
            assert!(receiver.observe(&ev.1), "{ev:?} is RPN #00/07");
            seen.push(Some(channel_of(&ev.1)));
        }
        assert_eq!(seen, channels.map(Some).collect::<Vec<_>>());
    }
    for channel in 0..16u8 {
        let want = match channel {
            1..=3 => 12.0,
            12..=14 => 24.0,
            _ => 48.0,
        };
        assert_eq!(receiver.semitones(channel), want, "channel {channel}");
    }
}

/// **A MIDI 2.0 Note On at velocity 0 binds its member channel** (M2-104
/// 7.4.2: it is a Note On; the translator before the ingest turns a MIDI 1.0
/// one into a Note Off): a channel bend there folds onto the note.
///
/// Mutation: bind only at a nonzero velocity (unbind at 0, as before the
/// translator moved) → the bend is dropped → fails.
#[test]
fn a_midi_2_note_on_at_velocity_0_binds_its_channel() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    let on0 = at(
        0,
        midi::note_on(MidiGroup::FIRST, MidiChannel::of(2), 60, 0),
    );
    assert_eq!(ingest.translated(&on0).collect::<Vec<_>>(), vec![on0]);
    let out = ingest
        .translated(&pitch_bend14(2, 16383))
        .next()
        .expect("the bend folds onto the note");
    assert_cv2!(out, _, Voice2::PerNotePitchBend { note, .. } if note.get() == 60);
}
