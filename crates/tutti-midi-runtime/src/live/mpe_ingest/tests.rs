use super::*;
use crate::test_support::at;
use tutti_fixtures::midi;
use tutti_graph::UmpAt;
use tutti_midi_protocol::{MpeZoneConfig, MpeZones};
use tutti_ump::{Address, CCNumber, KeyEvent, Message, Voice2};

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
        match $ev.1.decode() {
            Ok(Message::Midi2($addr, $pat)) $(if $guard)? => {}
            other => panic!("unexpected message: {other:?}"),
        }
    };
}

#[test]
fn member_channel_bend_becomes_per_note_pitch_bend() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
    // Note held on member channel 2.
    assert!(
        ingest
            .translated(&note_on(2, 60, 100))
            .next()
            .map(|(_, ump)| matches!(ump.key_event(), Some(KeyEvent::On(_))))
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
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
    // Master channel of a lower zone is channel 0.
    let out = ingest
        .translated(&pitch_bend14(0, 12288))
        .next()
        .expect("passes through");
    assert_cv2!(out, _, Voice2::PitchBend(_));
}

#[test]
fn member_channel_cc74_becomes_per_note_controller() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
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
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
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
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
    // No note on ch2 yet → member-channel bend has nothing to address.
    assert!(ingest.translated(&pitch_bend14(2, 16383)).next().is_none());
}

#[test]
fn native_per_note_passes_through() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
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
    let mut ingest = MpeIngest::new(MpeMode::SingleChannelRotation {
        channel: MidiChannel::of(5),
    });
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
    let mcm = at(
        0,
        MpeZoneConfig::lower(
            7,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap()
        .to_mcm(),
    );

    // The MCM itself is absorbed (configuration, not a musical event);
    // what goes on is the zone's reset ranges, all registered controllers
    // (`an_mcm_resets_its_zones_bend_ranges`).
    let reset: Vec<UmpAt> = ingest.translated(&mcm).collect();
    assert_eq!(
        ranges_only(reset.iter().copied()).len(),
        15,
        "the master's range, and on each of seven members the channel's and the per-note one"
    );
    assert!(!reset.contains(&mcm));
    assert!(
        matches!(ingest.mode_of(MidiGroup::FIRST), MpeMode::Zones(z) if z.lower().is_some() && z.upper().is_none())
    );

    // Now a member-channel bend folds to per-note, proving the zone is live.
    ingest.translated(&note_on(1, 60, 100));
    let out = ingest
        .translated(&pitch_bend14(1, 16383))
        .next()
        .expect("emits");
    assert_cv2!(out, _, Voice2::PerNotePitchBend { note, .. } if note.get() == 60);
}

/// **An MCM that would put both zones on one member channel is ignored**
/// (RP-053 makes the sender keep the zones apart; here the arrangement type
/// refuses the pair): absorbed, nothing sent, the zones as they were.
#[test]
fn an_mcm_that_would_overlap_the_zones_is_ignored() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(10));
    // 10 members up from channel 1 leave 4: an upper zone of 5 would share channel 10.
    let overlapping = at(
        0,
        MpeZoneConfig::upper(
            5,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap()
        .to_mcm(),
    );
    assert_eq!(
        ingest.translated(&overlapping).count(),
        0,
        "absorbed, nothing sent"
    );
    assert_eq!(
        *ingest.mode_of(MidiGroup::FIRST),
        tutti_fixtures::midi::lower_zone(10)
    );
    // One that fits is taken.
    let fits = at(
        0,
        MpeZoneConfig::upper(
            4,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap()
        .to_mcm(),
    );
    assert!(ingest.translated(&fits).count() > 0);
    assert_eq!(
        *ingest.mode_of(MidiGroup::FIRST),
        tutti_fixtures::midi::dual_zones(10, 4)
    );
}

#[test]
fn mcm_with_zero_members_disables_the_zone() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(7));
    // A raw MCM on master channel 0 with member_count == 0 disables the lower
    // zone. (`MpeZoneConfig::lower` refuses 0, so the disabling value can
    // only come off the wire: build it directly.)
    let disable = at(
        0,
        midi::registered_rpn(
            MidiGroup::FIRST,
            MidiChannel::FIRST, // lower-zone master channel
            Rpn::MPE_CONFIGURATION,
            0, // 0 members → disable
        ),
    );
    assert_eq!(
        ranges_only(ingest.translated(&disable)).len(),
        0,
        "the message is absorbed and a disabled zone has no ranges to reset"
    );
    assert!(matches!(
        ingest.mode_of(MidiGroup::FIRST),
        MpeMode::Disabled
    ));
}

/// What a receiver following RPN #00/00 (channel bend range) and
/// #00/07 (per-note bend range) per channel ends with after `events`:
/// `(channel, index) → semitones`, in order of arrival.
fn ranges_after(events: impl IntoIterator<Item = UmpAt>) -> Vec<(u8, u8, f32)> {
    events
        .into_iter()
        .map(|ev| {
            let Ok(Message::Midi2(address, Voice2::Registered { bank, index, value })) =
                ev.1.decode()
            else {
                panic!("{ev:?} is not a registered controller");
            };
            assert_eq!(bank.get(), Rpn::PITCH_BEND_SENSITIVITY.bank().get());
            let range = PitchBendSensitivity::from_rpn_bits(value.get());
            (address.channel.get(), index.get(), range.semitones().get())
        })
        .collect()
}

/// The Registered Controllers among `events`, the zone's ranges, without the
/// stops (All Sound Off, Reset All Controllers) that go with a layout change.
fn ranges_only(events: impl IntoIterator<Item = UmpAt>) -> Vec<UmpAt> {
    events.into_iter().filter(|ev| !is_stop(ev)).collect()
}

/// Whether `ev` is an All Sound Off or a Reset All Controllers.
fn is_stop(ev: &UmpAt) -> bool {
    matches!(
        ev.1.decode(),
        Ok(Message::Midi2(_, Voice2::ControlChange { controller, .. }))
            if controller == CCNumber::ALL_SOUND_OFF || controller == CCNumber::RESET_ALL
    )
}

/// The channels `events` stop, in order: each as `(channel, All Sound Off
/// then Reset All Controllers)`, asserted to come in that pair.
fn stopped(events: impl IntoIterator<Item = UmpAt>) -> Vec<u8> {
    let stops: Vec<UmpAt> = events.into_iter().filter(is_stop).collect();
    assert_eq!(stops.len() % 2, 0, "stops come in pairs");
    stops
        .chunks(2)
        .map(|pair| {
            let kind = |ev: &UmpAt| match ev.1.decode() {
                Ok(Message::Midi2(_, Voice2::ControlChange { controller, .. })) => controller,
                other => panic!("{other:?}"),
            };
            assert_eq!(kind(&pair[0]), CCNumber::ALL_SOUND_OFF);
            assert_eq!(kind(&pair[1]), CCNumber::RESET_ALL);
            assert_eq!(channel_of(&pair[0].1), channel_of(&pair[1].1));
            channel_of(&pair[0].1)
        })
        .collect()
}

/// **An MPE Configuration Message resets its zone's bend ranges** (MPE,
/// RP-053 / M1-100-UM, Pitch Bend Sensitivity: the master channel to
/// ±2, every member to ±48): an upper-zone MCM of three members yields
/// RPN #00/00 = 2 on channel 15 and RPN #00/07 = 48 on 12, 13 and 14, at
/// the message's offset, and a per-note receiver that had been told ±12
/// on those members is back at ±48. The other zone of a dual setup keeps
/// its ranges (nothing is sent for it), and a disabling MCM sends nothing.
/// A host-set mode sends the same through `zone_defaults`, for each zone.
#[test]
fn an_mcm_resets_its_zones_bend_ranges() {
    use tutti_fixtures::midi::BendRanges;
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(3));
    let mut receiver = BendRanges::default();
    for ev in ingest.translated(&bend_sensitivity(1, 12)) {
        receiver.observe(&ev.1);
    }
    let mcm = at(
        9,
        MpeZoneConfig::upper(
            3,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap()
        .to_mcm(),
    );
    let reset: Vec<UmpAt> = ingest.translated(&mcm).collect();
    assert!(matches!(
        ingest.mode_of(MidiGroup::FIRST),
        MpeMode::Zones(_)
    ));
    assert!(reset.iter().all(|ev| ev.0.get() == 9));
    assert_eq!(
        ranges_after(ranges_only(reset.iter().copied())),
        vec![
            (15, 0, 2.0),
            (12, 0, 2.0),
            (13, 0, 2.0),
            (14, 0, 2.0),
            (12, 7, 48.0),
            (13, 7, 48.0),
            (14, 7, 48.0)
        ]
    );
    // An upper member set to ±12, then the upper MCM again: the upper
    // members are back at ±48, the lower zone's ±12 stands.
    let mcm = at(
        0,
        MpeZoneConfig::upper(
            3,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap()
        .to_mcm(),
    );
    for ev in ingest.translated(&bend_sensitivity(13, 12)) {
        receiver.observe(&ev.1);
    }
    assert_eq!(receiver.semitones(tutti_ump::MidiChannel::of(13)), 12.0);
    for ev in ingest.translated(&mcm) {
        receiver.observe(&ev.1);
    }
    for channel in 12..=14 {
        assert_eq!(
            receiver.semitones(tutti_ump::MidiChannel::of(channel)),
            48.0,
            "channel {channel}"
        );
    }
    assert_eq!(
        receiver.semitones(tutti_ump::MidiChannel::of(1)),
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
    assert_eq!(
        ranges_only(ingest.translated(&disable)).len(),
        0,
        "a disabling MCM resets no range"
    );

    // A host-set dual-zone mode: each zone's ranges, lower first.
    ingest.set_mode(tutti_fixtures::midi::dual_zones(2, 1));
    let defaults = ranges_only(ingest.defaults(MidiGroup::FIRST, Offset::ZERO));
    assert_eq!(defaults.len(), 8);
    assert_eq!(
        ranges_after(defaults),
        vec![
            (0, 0, 2.0),
            (1, 0, 2.0),
            (2, 0, 2.0),
            (1, 7, 48.0),
            (2, 7, 48.0),
            (15, 0, 2.0),
            (14, 0, 2.0),
            (14, 7, 48.0)
        ]
    );
    ingest.set_mode(MpeMode::Disabled);
    assert_eq!(
        ranges_only(ingest.defaults(MidiGroup::FIRST, Offset::ZERO)).len(),
        0
    );
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
#[test]
fn a_member_channels_bend_range_becomes_every_members_per_note_range() {
    use tutti_fixtures::midi::BendRanges;
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::dual_zones(3, 3));
    let mut receiver = BendRanges::default();

    let master = bend_sensitivity(0, 2);
    let out: Vec<UmpAt> = ingest.translated(&master).collect();
    assert_eq!(out[0], master, "the master's own range passes through");
    assert_eq!(
        ranges_after(out[1..].iter().copied()),
        vec![(1, 0, 2.0), (2, 0, 2.0), (3, 0, 2.0)],
        "and is each member's channel range, which carries the master's bend"
    );
    assert!(out.iter().all(|ev| !receiver.observe(&ev.1)));

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
        assert_eq!(
            receiver.semitones(tutti_ump::MidiChannel::of(channel)),
            want,
            "channel {channel}"
        );
    }
}

/// **A MIDI 2.0 Note On at velocity 0 binds its member channel** (M2-104
/// 7.4.2: it is a Note On; the translator before the ingest turns a MIDI 1.0
/// one into a Note Off): a channel bend there folds onto the note.
#[test]
fn a_midi_2_note_on_at_velocity_0_binds_its_channel() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
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

/// **A Group's member channels, and the notes they hold, are its own** (M2-104
/// section 2.1.2, Appendix B.1): a note held on member channel 2 of Group 2 is
/// not the note a bend on channel 2 of Group 1 is for (that bend holds no note
/// in its Group and is dropped), and the per-note message built for Group 2's
/// bend stays in Group 2.
/// Spec: m2-104/2.1.2#2
#[test]
fn a_groups_member_channels_hold_notes_of_their_own() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
    let group2 = MidiGroup::of(1);
    let channel = MidiChannel::of(2);
    let bend = tutti_fixtures::midi::wide_bend(16383);
    ingest.translated(&at(
        0,
        midi::note_on(group2, channel, 60, midi::wide_velocity(100)),
    ));
    let first_group = at(0, midi::pitch_bend(MidiGroup::FIRST, channel, bend));
    assert_eq!(
        ingest.translated(&first_group).count(),
        0,
        "Group 1's channel 2 holds no note"
    );
    let second_group = at(0, midi::pitch_bend(group2, channel, bend));
    let out = ingest.translated(&second_group).next().expect("emits");
    assert_cv2!(
        out,
        Address { group, .. },
        Voice2::PerNotePitchBend { note, .. } if note.get() == 60 && group == group2
    );
}

/// **An MPE Configuration Message configures the Group it arrives in** and no
/// other (M2-104 Appendix B.1: what a message does in one Group does not
/// apply to Channels in any other Group), and its reset ranges go out on
/// that Group.
#[test]
fn an_mcm_configures_its_own_group_only() {
    let mut ingest = MpeIngest::new(MpeMode::Disabled);
    let group2 = MidiGroup::of(1);
    let mcm = at(
        0,
        midi::registered_rpn(
            group2,
            MidiChannel::FIRST,
            Rpn::MPE_CONFIGURATION,
            7 << 25, // seven members
        ),
    );
    let reset: Vec<UmpAt> = ingest.translated(&mcm).collect();
    assert!(matches!(ingest.mode_of(group2), MpeMode::Zones(_)));
    assert!(matches!(
        ingest.mode_of(MidiGroup::FIRST),
        MpeMode::Disabled
    ));
    assert_eq!(
        ranges_only(reset.iter().copied()).len(),
        15,
        "the master's range, and on each of seven members the channel's and the per-note one"
    );
    assert!(
        reset.iter().all(|ev| ev.1.group() == Some(group2)),
        "the reset goes out on the Group that asked"
    );
}

// ---- the state a note starts with (M1-100 sections 2.2.6 to 2.2.8) ----------

fn note_off(channel: u8, note: u8) -> UmpAt {
    at(
        0,
        midi::note_off(MidiGroup::FIRST, MidiChannel::of(channel), note, 0),
    )
}

fn pressure(channel: u8, value: u32) -> UmpAt {
    at(
        0,
        midi::channel_pressure(MidiGroup::FIRST, MidiChannel::of(channel), value),
    )
}

/// What `ingest` sends for `event`, as decoded `(channel, message)` pairs.
fn decoded(ingest: &mut MpeIngest, event: &UmpAt) -> Vec<(u8, Voice2)> {
    ingest
        .translated(event)
        .map(|ev| match ev.1.decode() {
            Ok(Message::Midi2(address, voice)) => (address.channel.get(), voice),
            other => panic!("unexpected message: {other:?}"),
        })
        .collect()
}

/// **A bend received before the Note On is the new note's** (M1-100 section
/// 2.2.6: the pitch of a new note follows the Pitch Bend most recently
/// received on both the Manager Channel and that note's Member Channel before
/// Note On, and a receiver keeps tracking Pitch Bend while no note
/// sounds; section 2.4, Table 4: the sender's Pitch Bend comes first). The ingest remembers the bend of the member channel with no note,
/// and the note that starts there is told it ahead of its Note On; the last of
/// two bends is the one.
#[test]
fn a_member_bend_before_the_note_on_is_the_notes_start() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
    assert!(decoded(&mut ingest, &pitch_bend14(2, 9000)).is_empty());
    assert!(decoded(&mut ingest, &pitch_bend14(2, 16383)).is_empty());
    let out = decoded(&mut ingest, &note_on(2, 60, 100));
    assert_eq!(out.len(), 2, "the bend, then the note: {out:?}");
    assert!(matches!(
        out[0],
        (2, Voice2::PerNotePitchBend { note, value })
            if note.get() == 60 && value == BendValue::new(tutti_fixtures::midi::wide_bend(16383))
    ));
    assert!(matches!(out[1], (2, Voice2::NoteOn { .. })));
}

/// **Channel Pressure and Control Change #74 received before the Note On are
/// the new note's** (M1-100 section 2.2.7: the control of a new note follows the
/// Channel Pressure most recently received on its Channel before Note On, and
/// a receiver keeps tracking it while no note sounds; section 2.2.8 the same of Control
/// Change #74).
#[test]
fn member_pressure_and_cc74_before_the_note_on_are_the_notes_start() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
    assert!(decoded(&mut ingest, &cc(3, CCNumber::BRIGHTNESS, 64)).is_empty());
    assert!(decoded(&mut ingest, &pressure(3, 0x4000_0000)).is_empty());
    let out = decoded(&mut ingest, &note_on(3, 64, 100));
    assert_eq!(out.len(), 3, "{out:?}");
    assert!(matches!(
        out[0],
        (3, Voice2::PolyPressure { note, value }) if note.get() == 64 && value == ControlValue::new(0x4000_0000)
    ));
    assert!(matches!(
        out[1],
        (3, Voice2::AssignablePerNote { note, controller: 74, value })
            if note.get() == 64 && value == ControlValue::new(tutti_fixtures::midi::wide_value(64))
    ));
    assert!(matches!(out[2], (3, Voice2::NoteOn { .. })));
}

/// What a member channel was told stays its own: another member's note does
/// not start with it, and a channel never told sends no per-note message
/// ahead of its Note On.
#[test]
fn what_a_member_was_told_is_its_own() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
    ingest.translated(&pitch_bend14(2, 16383));
    let out = decoded(&mut ingest, &note_on(3, 60, 100));
    assert!(
        matches!(out.as_slice(), [(3, Voice2::NoteOn { .. })]),
        "{out:?}"
    );
}

/// **A released note stops taking the member's messages** (M1-100 section
/// 2.2.6: a released note ceases to be affected by Pitch Bend from the Member
/// Channels after its Note Off; section 2.4: MPE control messages do not
/// affect a note after its Note Off), while the channel goes on tracking them for
/// the next note.
#[test]
fn a_released_note_ignores_the_members_bend_and_the_next_note_starts_with_it() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(15));
    ingest.translated(&note_on(2, 60, 100));
    ingest.translated(&note_off(2, 60));
    assert!(decoded(&mut ingest, &pitch_bend14(2, 12000)).is_empty());
    let out = decoded(&mut ingest, &note_on(2, 62, 100));
    assert!(matches!(
        out[0],
        (2, Voice2::PerNotePitchBend { note, .. }) if note.get() == 62
    ));
}

/// **A zone-wide pressure or CC 74 reaches the notes of its members**
/// (M1-100 section 2.2.7: an MPE receiver responds to Channel Pressure on both
/// the Manager Channel and each Member Channel and combines the two
/// separately for each Sounding Note; section 2.2.8 the same
/// of Control Change #74; Appendix D: Max is one way). The manager's value,
/// told with no note sounding, is every new note's floor; told while notes
/// sound, it reaches each of them, as the greater of it and the member's own.
#[test]
fn a_managers_pressure_and_cc74_reach_every_members_note() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(3));
    // Told with no note sounding: the first note starts with it.
    assert_eq!(decoded(&mut ingest, &pressure(0, 0x2000_0000)).len(), 1);
    let out = decoded(&mut ingest, &note_on(1, 60, 100));
    assert!(matches!(
        out[0],
        (1, Voice2::PolyPressure { note, value }) if note.get() == 60 && value == ControlValue::new(0x2000_0000)
    ));
    ingest.translated(&note_on(2, 64, 100));
    // A member above the floor keeps its own; one below takes the floor.
    ingest.translated(&pressure(2, 0x7000_0000));
    ingest.translated(&pressure(1, 0x1000_0000));
    // The manager's pressure now moves: each held note hears the greater.
    let out = decoded(&mut ingest, &pressure(0, 0x4000_0000));
    let heard: Vec<(u8, u8, u32)> = out
        .iter()
        .filter_map(|(c, v)| match v {
            Voice2::PolyPressure { note, value } => Some((*c, note.get(), value.get())),
            _ => None,
        })
        .collect();
    assert_eq!(heard, [(1, 60, 0x4000_0000), (2, 64, 0x7000_0000)]);
    assert!(
        matches!(out.last(), Some((0, Voice2::ChannelPressure(v))) if v.get() == 0x4000_0000),
        "the manager's own message still passes"
    );
    // CC 74 is the same.
    let out = decoded(&mut ingest, &cc(0, CCNumber::BRIGHTNESS, 100));
    let slides = out
        .iter()
        .filter(|(_, v)| matches!(v, Voice2::AssignablePerNote { controller: 74, .. }))
        .count();
    assert_eq!(slides, 2);
}

// ---- Table 5: what a receiver takes where (M1-100 Appendix E) -------------

/// **A Prohibited message is ignored by a receiver** (M1-100 section 2.3.1,
/// on a message that is not for the channel it came on; Table 5, P for
/// prohibited). On the Manager Channel, All Sound Off
/// (120), Mono Mode On (126) and Poly Mode On (127); on every channel, Omni On
/// (125). Reset All Controllers (121), Local Control (122), All Notes Off
/// (123) and Omni Off (124) are Optional and pass, on the master and on a
/// member alike, as do 120, 126 and 127 on a member.
#[test]
fn the_receive_table_ignores_what_it_prohibits() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(5));
    let number = CCNumber::of;
    for (channel, controller, passes) in [
        (0, 120, false),
        (0, 126, false),
        (0, 127, false),
        (0, 125, false),
        (2, 125, false),
        (0, 121, true),
        (0, 122, true),
        (0, 123, true),
        (0, 124, true),
        (2, 120, true),
        (2, 121, true),
        (2, 123, true),
        (2, 124, true),
        (2, 127, true),
        // Channel 9 is in no zone: conventional use.
        (9, 120, true),
        (9, 125, true),
    ] {
        let sent = cc(channel, number(controller), 0);
        let out: Vec<UmpAt> = ingest.translated(&sent).collect();
        assert_eq!(
            out.contains(&sent),
            passes,
            "CC {controller} on channel {channel}"
        );
    }
}

/// **The MPE Configuration Message is not for a Member Channel** (Table 5
/// marks RPN #6 prohibited on Member Channels): on a member that is not a master's channel
/// it is ignored, and the zones stand. On a channel outside every zone, RPN 6
/// is ordinary traffic.
#[test]
fn an_mcm_on_a_member_channel_is_ignored() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(5));
    let mcm_on = |channel: u8| {
        at(
            0,
            midi::registered_rpn(
                MidiGroup::FIRST,
                MidiChannel::of(channel),
                Rpn::MPE_CONFIGURATION,
                3 << 25,
            ),
        )
    };
    assert_eq!(ingest.translated(&mcm_on(3)).count(), 0);
    assert_eq!(ingest.translated(&mcm_on(9)).count(), 1);
    assert_eq!(
        *ingest.mode_of(MidiGroup::FIRST),
        tutti_fixtures::midi::lower_zone(5)
    );
}

/// **MIDI Mode 3 is the default and Program Change on a Member Channel is
/// ignored in it; Mode 4 is switched on the lowest Member Channel**
/// (M1-100 section 2.3.3: a receiver in MIDI Mode 3 ignores Program Change on
/// Member Channels; Table 5, Bank Select on Member Channels is prohibited in
/// Mode 3 and optional in Mode 4; section 2.2.4.3: the Mode Message goes to
/// the lowest numbered Member Channel of a zone, not the Manager Channel). Mono Mode On on
/// the manager, or on another member, switches nothing; on the lowest member
/// (channel 1 of a lower zone) it lets members' Program Changes through, and
/// Poly Mode On there takes them away again. The manager's Program Change
/// always passes.
#[test]
fn program_change_on_a_member_follows_the_zones_midi_mode() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(5));
    let program = |channel: u8| {
        at(
            0,
            midi::program_change(MidiGroup::FIRST, MidiChannel::of(channel), 5, None),
        )
    };
    assert_eq!(ingest.translated(&program(3)).count(), 0, "Mode 3");
    assert_eq!(ingest.translated(&program(0)).count(), 1, "the manager's");
    ingest.translated(&cc(0, CCNumber::of(126), 0));
    ingest.translated(&cc(3, CCNumber::of(126), 0));
    assert_eq!(
        ingest.translated(&program(3)).count(),
        0,
        "neither the manager nor another member switches the mode"
    );
    ingest.translated(&cc(1, CCNumber::of(126), 0));
    assert_eq!(ingest.translated(&program(3)).count(), 1, "Mode 4");
    ingest.translated(&cc(1, CCNumber::of(127), 0));
    assert_eq!(ingest.translated(&program(3)).count(), 0, "Mode 3 again");
}

/// **An MPE 1.1 zone starts in the mode its response names**: a Monophonic
/// zone is in MIDI Mode 4, so its members' Program Changes pass without a Mono
/// Mode On, and Poly Mode On from the wire still takes them away (M1-100
/// section 2.2.4.3).
#[test]
fn an_mpe_1_1_zone_starts_in_the_mode_its_response_names() {
    let zone = MpeZoneConfig::lower(
        5,
        PitchBendSensitivity::MPE_MASTER_DEFAULT,
        PitchBendSensitivity::MPE_MEMBER_DEFAULT,
    )
    .unwrap()
    .with_channel_response(ChannelResponse::Monophonic);
    let mut ingest = MpeIngest::new(MpeMode::Zones(MpeZones::single(zone)));
    let program = at(
        0,
        midi::program_change(MidiGroup::FIRST, MidiChannel::of(3), 5, None),
    );
    assert_eq!(ingest.translated(&program).count(), 1, "Mode 4");
    ingest.translated(&cc(1, CCNumber::of(127), 0));
    assert_eq!(ingest.translated(&program).count(), 0, "Mode 3 again");
}

/// **A channel in no zone is for conventional use** (M1-100 section 2.2.1:
/// a Channel in no Zone remains available for conventional use): notes, bends, pressure and CC 74 there pass as they
/// came, whatever the zones are.
#[test]
fn a_channel_outside_the_zones_passes_everything() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(5));
    for event in [
        note_on(9, 60, 100),
        pitch_bend14(9, 12000),
        pressure(9, 7),
        cc(9, CCNumber::BRIGHTNESS, 7),
        note_off(9, 60),
    ] {
        assert_eq!(ingest.translated(&event).collect::<Vec<_>>(), vec![event]);
    }
}

// ---- changing the zones (M1-100 section 2.2.3) ------------------------------

/// **A zone change stops and resets the channels it moves** (M1-100 section
/// 2.2.3: a receiver that changes its Zone configurations stops all Sounding
/// Notes and resets all controls on each Channel entering or leaving MPE
/// control). Growing a lower zone from
/// 3 to 5 members takes channels 4 and 5 into it: an MCM from the wire stops
/// those two (All Sound Off, then Reset All Controllers) and no other; the
/// notes the other channels hold, and what they were told, stand; the same MCM
/// again changes nothing and stops nothing.
#[test]
fn an_mcm_stops_only_the_channels_it_moves() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(3));
    ingest.translated(&note_on(2, 60, 100));
    ingest.translated(&pitch_bend14(2, 9000));
    // Channel 4 is conventional: its note and its bend are the instrument's.
    ingest.translated(&note_on(4, 62, 100));
    ingest.translated(&pitch_bend14(4, 9000));

    let grow = at(
        0,
        MpeZoneConfig::lower(
            5,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap()
        .to_mcm(),
    );
    let out: Vec<UmpAt> = ingest.translated(&grow).collect();
    assert_eq!(stopped(out.iter().copied()), [4, 5]);
    assert_eq!(ranges_only(out).len(), 11, "and the zone's ranges");

    // Channel 2 kept its note: a bend still folds onto it.
    let bent = decoded(&mut ingest, &pitch_bend14(2, 16383));
    assert!(matches!(
        bent.as_slice(),
        [(2, Voice2::PerNotePitchBend { note, .. })] if note.get() == 60
    ));
    // Channel 4 holds nothing, and was told nothing: its note starts alone.
    let on = decoded(&mut ingest, &note_on(4, 64, 100));
    assert!(
        matches!(on.as_slice(), [(4, Voice2::NoteOn { .. })]),
        "{on:?}"
    );

    assert_eq!(
        stopped(ingest.translated(&grow)),
        Vec::<u8>::new(),
        "the same MCM again moves nothing"
    );
}

/// **A channel that left MPE control forgets its note and what it was told**
/// (section 2.2.3, controls reset on each Channel entering or leaving MPE
/// control): shrink a zone past a channel that held a note and had been
/// bent, grow it back, and the channel starts clean.
#[test]
fn a_channel_that_left_the_zone_forgets_its_note_and_its_bend() {
    let regrown = || {
        let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(5));
        ingest.translated(&note_on(4, 62, 100));
        ingest.translated(&pitch_bend14(4, 9000));
        ingest.translated(&at(
            0,
            MpeZoneConfig::lower(
                3,
                PitchBendSensitivity::MPE_MASTER_DEFAULT,
                PitchBendSensitivity::MPE_MEMBER_DEFAULT,
            )
            .unwrap()
            .to_mcm(),
        ));
        ingest.translated(&at(
            0,
            MpeZoneConfig::lower(
                5,
                PitchBendSensitivity::MPE_MASTER_DEFAULT,
                PitchBendSensitivity::MPE_MEMBER_DEFAULT,
            )
            .unwrap()
            .to_mcm(),
        ));
        ingest
    };
    // No stale note to fold a bend onto.
    assert!(decoded(&mut regrown(), &pitch_bend14(4, 12000)).is_empty());
    // No stale bend to start the next note with.
    let on = decoded(&mut regrown(), &note_on(4, 64, 100));
    assert!(
        matches!(on.as_slice(), [(4, Voice2::NoteOn { .. })]),
        "{on:?}"
    );
}

/// A disabling MCM stops the zone's channels, the master's too, and its
/// notes are forgotten (section 2.2.3, entering or leaving MPE control).
#[test]
fn a_disabling_mcm_stops_every_channel_of_the_zone() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(2));
    ingest.translated(&note_on(1, 60, 100));
    let disable = at(
        0,
        midi::registered_rpn(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            Rpn::MPE_CONFIGURATION,
            0,
        ),
    );
    assert_eq!(stopped(ingest.translated(&disable)), [0, 1, 2]);
    // Channel 1 is conventional again: nothing of the zone is left to fold.
    let bend = pitch_bend14(1, 9000);
    assert_eq!(ingest.translated(&bend).collect::<Vec<_>>(), vec![bend]);
}

/// A host-set zone change (`set_mode`) stops the channels that enter or leave
/// MPE control, in what `zone_defaults` sends, and keeps the notes of those
/// that did not: lower zone of 3 members to one of 2 stops channel 3 alone;
/// to Disabled stops channels 0 to 2; the notes of the unchanged channels were
/// kept until then (section 2.2.3).
#[test]
fn a_host_set_mode_stops_the_channels_that_enter_or_leave() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(3));
    ingest.translated(&note_on(1, 60, 100));
    ingest.set_mode(tutti_fixtures::midi::lower_zone(2));
    let sent = ingest.defaults(MidiGroup::FIRST, Offset::ZERO);
    assert_eq!(stopped(sent), [3]);
    let bent = decoded(&mut ingest, &pitch_bend14(1, 16383));
    assert!(
        matches!(bent.as_slice(), [(1, Voice2::PerNotePitchBend { .. })]),
        "channel 1 kept its note: {bent:?}"
    );
    ingest.set_mode(MpeMode::Disabled);
    let sent = ingest.defaults(MidiGroup::FIRST, Offset::ZERO);
    assert_eq!(stopped(sent), [0, 1, 2]);
}

/// A host-set mode in force changes nothing and stops nothing.
#[test]
fn the_mode_in_force_stops_nothing() {
    let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(3));
    ingest.set_mode(tutti_fixtures::midi::lower_zone(3));
    assert_eq!(
        stopped(ingest.defaults(MidiGroup::FIRST, Offset::ZERO)),
        Vec::<u8>::new()
    );
}

// --- The MIDI-CI MPE Profile (M2-120) ---

fn zone(manager: u8, channels: u8) -> MpeZoneConfig {
    MpeZoneConfig::profile(
        MidiChannel::of(manager),
        channels,
        PitchBendSensitivity::MPE_PROFILE_DEFAULT,
    )
    .expect("a zone that fits")
}

fn enable(ingest: &mut MpeIngest, zones: MpeZones) -> Vec<UmpAt> {
    let mut out = Vec::new();
    ingest.set_profile(MidiGroup::FIRST, Some(zones), Offset::ZERO, |e| out.push(e));
    out
}

fn disable(ingest: &mut MpeIngest) -> Vec<UmpAt> {
    let mut out = Vec::new();
    ingest.set_profile(MidiGroup::FIRST, None, Offset::ZERO, |e| out.push(e));
    out
}

/// An ingest of no MPE 1.1 zones with the Profile zone enabled.
fn profiled(manager: u8, channels: u8) -> MpeIngest {
    let mut ingest = MpeIngest::new(MpeMode::Disabled);
    enable(&mut ingest, MpeZones::single(zone(manager, channels)));
    ingest
}

/// The registered controllers among `events` at `rpn`, as `(channel, data)`.
fn rpns(events: impl IntoIterator<Item = UmpAt>, rpn: Rpn) -> Vec<(u8, u32)> {
    events
        .into_iter()
        .filter_map(|ev| match ev.1.decode() {
            Ok(Message::Midi2(address, Voice2::Registered { bank, index, value }))
                if Rpn::new(bank, index) == rpn =>
            {
                Some((address.channel.get(), value.get()))
            }
            _ => None,
        })
        .collect()
}

/// **Enabling the Profile starts the manager and every member at 48** (M2-120
/// section 3.5, where MPE 1.1's manager starts at 2): RPN #00/00 = 48 on the
/// manager, which passes as its own range, and RPN #00/07 = 48 on each
/// member, whose bends are per-note.
#[test]
fn enabling_the_profile_starts_every_channel_at_48_semitones() {
    let mut ingest = MpeIngest::new(MpeMode::Disabled);
    let out = enable(&mut ingest, MpeZones::single(zone(2, 4)));
    let forty_eight = PitchBendSensitivity::of(48).to_rpn_bits();
    assert_eq!(
        rpns(out.iter().copied(), Rpn::PITCH_BEND_SENSITIVITY),
        [
            (2, forty_eight),
            (3, forty_eight),
            (4, forty_eight),
            (5, forty_eight)
        ],
        "the manager's, and each member's channel range under it"
    );
    assert_eq!(
        rpns(out.iter().copied(), Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY),
        [(3, forty_eight), (4, forty_eight), (5, forty_eight)]
    );

    // MPE 1.1, by contrast, starts the manager at 2.
    let mpe11 = MpeIngest::new(midi::lower_zone(3));
    let out: Vec<UmpAt> = mpe11.defaults(MidiGroup::FIRST, Offset::ZERO).collect();
    let two = PitchBendSensitivity::of(2).to_rpn_bits();
    assert_eq!(
        rpns(out, Rpn::PITCH_BEND_SENSITIVITY),
        [(0, two), (1, two), (2, two), (3, two)]
    );
}

/// **A receiver stops the sounding notes and resets the controls of each
/// channel that enters or leaves MPE control** (M2-120 section 3.3): the
/// zone's channels on enabling, and on disabling the same.
#[test]
fn entering_and_leaving_the_profile_stops_and_resets_the_zones_channels() {
    let mut ingest = MpeIngest::new(MpeMode::Disabled);
    let entering = enable(&mut ingest, MpeZones::single(zone(5, 3)));
    assert_eq!(stopped(entering), [5, 6, 7]);
    let leaving = disable(&mut ingest);
    assert_eq!(stopped(leaving), [5, 6, 7]);
    assert_eq!(disable(&mut ingest).len(), 0, "nothing more to leave");
}

/// The same zones again are no change, and a second zone elsewhere stops
/// only its own channels.
#[test]
fn only_the_channels_a_change_moves_are_stopped() {
    let mut ingest = MpeIngest::new(MpeMode::Disabled);
    let low = MpeZones::single(zone(0, 4));
    enable(&mut ingest, low);
    assert_eq!(enable(&mut ingest, low).len(), 0, "the same zones");
    let both = low.with(zone(8, 4)).expect("disjoint");
    assert_eq!(stopped(enable(&mut ingest, both)), [8, 9, 10, 11]);
    assert_eq!(ingest.profile(MidiGroup::FIRST), Some(&both));
}

/// **Table 15, the Rx columns, for the Channel Mode rows**: All Sounds Off is
/// taken on both roles; Reset All Controllers and All Notes Off are the
/// Manager's alone; Omni Off, Omni On, Mono Mode On and Poly Mode On are
/// prohibited on both. A channel in no zone is conventional.
#[test]
fn the_profile_receive_table_for_channel_mode_messages() {
    let mut ingest = profiled(0, 5);
    let number = CCNumber::of;
    for (channel, controller, passes) in [
        (0, 120, true),
        (0, 121, true),
        (0, 123, true),
        (0, 124, false),
        (0, 125, false),
        (0, 126, false),
        (0, 127, false),
        (2, 120, true),
        (2, 121, false),
        (2, 123, false),
        (2, 124, false),
        (2, 125, false),
        (2, 126, false),
        (2, 127, false),
        (9, 120, true),
        (9, 121, true),
        (9, 123, true),
        (9, 124, true),
        (9, 125, true),
        (9, 126, true),
        (9, 127, true),
    ] {
        let sent = cc(channel, number(controller), 0);
        let out: Vec<UmpAt> = ingest.translated(&sent).collect();
        assert_eq!(
            out.contains(&sent),
            passes,
            "CC {controller} on channel {channel}"
        );
    }
}

/// MPE 1.1 and the Profile disagree on All Sounds Off on the Manager Channel
/// (prohibited under Table 5, taken under Table 15) and on Reset All
/// Controllers on a Member Channel (optional, then prohibited): one ingest,
/// each rule set in turn.
#[test]
fn the_two_specifications_receive_different_tables() {
    let mut ingest = MpeIngest::new(midi::lower_zone(5));
    let sound_off = cc(0, CCNumber::ALL_SOUND_OFF, 0);
    let reset = cc(2, CCNumber::RESET_ALL, 0);
    assert_eq!(ingest.translated(&sound_off).count(), 0, "MPE 1.1");
    assert_eq!(ingest.translated(&reset).count(), 1, "MPE 1.1");
    enable(&mut ingest, MpeZones::single(zone(0, 6)));
    assert!(
        ingest.translated(&sound_off).any(|e| e == sound_off),
        "the Profile"
    );
    assert_eq!(ingest.translated(&reset).count(), 0, "the Profile");
    disable(&mut ingest);
    assert_eq!(ingest.translated(&sound_off).count(), 0, "MPE 1.1 again");
    assert_eq!(ingest.translated(&reset).count(), 1, "MPE 1.1 again");
}

/// **The sensitivity is set on the Manager and is every channel's** (M2-120
/// section 3.5, Table 15): the Manager's RPN 0 goes on as its own and reaches
/// every Member as a per-note sensitivity; a Member's is prohibited, so it
/// is ignored and moves nobody. MPE 1.1 does the opposite: the Member's
/// reaches the members and the Manager's moves no member.
#[test]
fn the_pitch_bend_sensitivity_is_the_managers() {
    let sensitivity = |channel: u8, semitones: u8| {
        at(
            0,
            midi::registered_rpn(
                MidiGroup::FIRST,
                MidiChannel::of(channel),
                Rpn::PITCH_BEND_SENSITIVITY,
                PitchBendSensitivity::of(semitones).to_rpn_bits(),
            ),
        )
    };
    let twelve = PitchBendSensitivity::of(12).to_rpn_bits();
    let mut ingest = profiled(0, 4);
    let out: Vec<UmpAt> = ingest.translated(&sensitivity(0, 12)).collect();
    assert_eq!(
        rpns(out.iter().copied(), Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY),
        [(1, twelve), (2, twelve), (3, twelve)]
    );
    assert_eq!(
        rpns(out, Rpn::PITCH_BEND_SENSITIVITY),
        [(0, twelve), (1, twelve), (2, twelve), (3, twelve)],
        "and the manager's own passes, copied onto each member"
    );
    assert_eq!(ingest.translated(&sensitivity(2, 7)).count(), 0);

    let mut mpe11 = MpeIngest::new(midi::lower_zone(3));
    let out: Vec<UmpAt> = mpe11.translated(&sensitivity(2, 12)).collect();
    assert_eq!(
        rpns(out, Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY).len(),
        3,
        "MPE 1.1: the member's is the members'"
    );
    assert_eq!(
        rpns(
            mpe11.translated(&sensitivity(0, 12)),
            Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY
        )
        .len(),
        0,
        "MPE 1.1: the manager's is the manager's"
    );
}

/// **Program Change and Bank Select follow the Channel Response** (M2-120
/// section 4.6, Table 15): the Manager's is always taken; a Member's is
/// ignored in Polyphonic Channel Response and may be taken in Monophonic.
/// Mono Mode On on the lowest Member Channel, MPE 1.1's switch, is prohibited
/// and does nothing.
#[test]
fn program_change_on_a_member_follows_the_channel_response() {
    let program = |channel: u8| {
        at(
            0,
            midi::program_change(MidiGroup::FIRST, MidiChannel::of(channel), 5, None),
        )
    };
    let mut poly = profiled(0, 4);
    assert_eq!(poly.translated(&program(0)).count(), 1, "the manager's");
    assert_eq!(poly.translated(&program(2)).count(), 0, "polyphonic");
    poly.translated(&cc(1, CCNumber::of(126), 0));
    assert_eq!(poly.translated(&program(2)).count(), 0, "no Mode switch");

    let mut mono = MpeIngest::new(MpeMode::Disabled);
    enable(
        &mut mono,
        MpeZones::single(zone(0, 4).with_channel_response(ChannelResponse::Monophonic)),
    );
    assert_eq!(mono.translated(&program(0)).count(), 1, "the manager's");
    assert_eq!(mono.translated(&program(2)).count(), 1, "monophonic");
}

/// **The Manager is the zone's lowest channel, wherever that is** (M2-120
/// section 2.2): a zone managed from channel 5 folds its Members' expression
/// into per-note messages and passes its Manager's, and the channels below
/// it are conventional.
#[test]
fn a_zone_managed_from_any_channel_folds_its_members() {
    let mut ingest = profiled(5, 4);
    ingest.translated(&note_on(7, 60, 100));
    let out = ingest
        .translated(&pitch_bend14(7, 16383))
        .next()
        .expect("a per-note bend");
    assert_cv2!(out, Address { channel, .. }, Voice2::PerNotePitchBend { note, .. }
        if channel.get() == 7 && note.get() == 60);
    let out = ingest
        .translated(&pitch_bend14(5, 12288))
        .next()
        .expect("the manager's passes");
    assert_cv2!(out, _, Voice2::PitchBend(_));
    // Channel 2 is in no zone, and 9 is past the zone's last channel (8).
    for channel in [2, 9] {
        let out = ingest
            .translated(&pitch_bend14(channel, 16383))
            .next()
            .expect("passes");
        assert_cv2!(out, _, Voice2::PitchBend(_));
    }
}

/// **The controls a Member was told before its note go ahead of the Note On**
/// (M2-120 sections 4.3 to 4.5, as MPE 1.1's 2.2.6 to 2.2.8): Pitch Bend,
/// Channel Pressure and Control Change #74, tracked with no note sounding and
/// combined with the Manager's.
#[test]
fn what_a_member_was_told_goes_ahead_of_its_note_on() {
    let mut ingest = profiled(0, 4);
    assert_eq!(ingest.translated(&pitch_bend14(2, 12000)).count(), 0);
    ingest.translated(&cc(2, CCNumber::BRIGHTNESS, 40));
    ingest.translated(&at(
        0,
        midi::channel_pressure(MidiGroup::FIRST, MidiChannel::of(0), 0x4000_0000),
    ));
    let out: Vec<UmpAt> = ingest.translated(&note_on(2, 60, 100)).collect();
    assert_eq!(out.len(), 4, "bend, pressure, CC 74, then the note");
    assert_cv2!(out[0], _, Voice2::PerNotePitchBend { note, .. } if note.get() == 60);
    assert_cv2!(out[1], _, Voice2::PolyPressure { note, .. } if note.get() == 60);
    assert_cv2!(out[2], _, Voice2::AssignablePerNote { note, controller: 74, .. }
        if note.get() == 60);
    assert!(matches!(out[3].1.key_event(), Some(KeyEvent::On(_))));
}

/// **MPE 1.1's Configuration Message has no part in the Profile** (M2-120
/// section 2.2): in a Group with Profile zones RPN #6 is ordinary traffic and
/// reconfigures nothing; once they are disabled it configures as before.
#[test]
fn a_configuration_message_does_nothing_while_the_profile_is_enabled() {
    let mut ingest = profiled(0, 4);
    let mcm = at(
        0,
        MpeZoneConfig::lower(
            7,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        )
        .unwrap()
        .to_mcm(),
    );
    assert_eq!(ingest.translated(&mcm).count(), 1, "passes, not absorbed");
    assert!(matches!(
        ingest.mode_of(MidiGroup::FIRST),
        MpeMode::Disabled
    ));
    disable(&mut ingest);
    assert_eq!(ranges_only(ingest.translated(&mcm)).len(), 15, "absorbed");
    assert!(matches!(
        ingest.mode_of(MidiGroup::FIRST),
        MpeMode::Zones(_)
    ));
}

/// **Disabling goes back to the layout MPE 1.1 had**, and starts its zones
/// at its ranges again (manager 2, members 48).
#[test]
fn disabling_the_profile_restores_the_mpe_11_zones_at_their_ranges() {
    let mut ingest = MpeIngest::new(midi::lower_zone(3));
    enable(&mut ingest, MpeZones::single(zone(4, 4)));
    let out = disable(&mut ingest);
    let two = PitchBendSensitivity::of(2).to_rpn_bits();
    assert_eq!(
        rpns(out.iter().copied(), Rpn::PITCH_BEND_SENSITIVITY),
        [(0, two), (1, two), (2, two), (3, two)]
    );
    assert_eq!(
        rpns(out.iter().copied(), Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY).len(),
        3
    );
    assert_eq!(stopped(out), [0, 1, 2, 3, 4, 5, 6, 7]);
    ingest.translated(&note_on(2, 60, 100));
    let out = ingest
        .translated(&pitch_bend14(2, 16383))
        .next()
        .expect("emits");
    assert_cv2!(out, _, Voice2::PerNotePitchBend { .. });
}

/// A Group is a channel space of its own: the Profile on one is not on another.
#[test]
fn a_group_has_its_own_profile_zones() {
    let mut ingest = profiled(0, 4);
    let other = MidiGroup::of(1);
    assert_eq!(ingest.profile(other), None);
    let bend = at(0, midi::pitch_bend(other, MidiChannel::of(2), 0xFFFF_FFFF));
    let out = ingest.translated(&bend).next().expect("passes");
    assert_cv2!(out, _, Voice2::PitchBend(_));
}

/// Notes on the Manager Channel are undefined by the Profile (Table 15, U), so
/// they pass as for any channel and bind no member.
#[test]
fn a_note_on_the_manager_channel_passes() {
    let mut ingest = profiled(0, 4);
    let out = ingest
        .translated(&note_on(0, 60, 100))
        .next()
        .expect("passes");
    assert!(matches!(out.1.key_event(), Some(KeyEvent::On(_))));
}

/// **A Manager Channel's zone-wide message is every channel of its zone's**
/// (M1-100 section 2.3.1: certain messages, the Damper Pedal for one, affect
/// all sounding notes across the Manager Channel and all Member Channels, and
/// are sent only on the Manager Channel): the pedals, All Notes Off and Reset
/// All Controllers go on, and are sent on each member channel with the same
/// value, ahead of nothing and after the manager's own. A channel outside the
/// zone hears nothing.
#[test]
fn a_managers_zone_wide_message_is_sent_on_every_member_channel() {
    for controller in [
        CCNumber::SUSTAIN,
        CCNumber::SOSTENUTO,
        CCNumber::ALL_NOTES_OFF,
        CCNumber::RESET_ALL,
    ] {
        let mut ingest = MpeIngest::new(midi::lower_zone(3));
        let sent = cc(0, controller, 127);
        let out = decoded(&mut ingest, &sent);
        let channels: Vec<u8> = out.iter().map(|(c, _)| *c).collect();
        assert_eq!(channels, [0, 1, 2, 3], "CC {}", controller.get());
        assert!(
            out.iter().all(|(_, v)| matches!(v,
                Voice2::ControlChange { controller: c, value }
                    if *c == controller && *value == ControlValue::new(u32::MAX))),
            "CC {}: {out:?}",
            controller.get()
        );
    }
}

/// The upper zone counts down from the channel below its manager: a pedal on
/// 15 is on 15 and on 13 and 14 (RP-053), and not on the lower zone's channels.
#[test]
fn the_upper_zones_pedal_reaches_its_own_members_only() {
    let mut ingest = MpeIngest::new(midi::dual_zones(2, 2));
    let out = decoded(&mut ingest, &cc(15, CCNumber::SUSTAIN, 127));
    let channels: Vec<u8> = out.iter().map(|(c, _)| *c).collect();
    assert_eq!(channels, [15, 13, 14]);
}

/// A member's pedal and panic are its own channel's alone (M1-100 section
/// 2.3.1: the zone-wide ones are sent only on the Manager Channel), and a
/// channel in no zone is conventional.
#[test]
fn a_members_message_and_a_conventional_channels_stay_on_their_channel() {
    let mut ingest = MpeIngest::new(midi::lower_zone(3));
    for channel in [2, 9] {
        for controller in [
            CCNumber::SUSTAIN,
            CCNumber::ALL_NOTES_OFF,
            CCNumber::RESET_ALL,
        ] {
            let sent = cc(channel, controller, 127);
            assert_eq!(
                ingest.translated(&sent).collect::<Vec<_>>(),
                vec![sent],
                "CC {} on channel {channel}",
                controller.get()
            );
        }
    }
}

/// The Profile takes All Sounds Off on its Manager (M2-120 Table 15) and it is
/// the zone's: sent on each member too.
#[test]
fn a_profile_managers_all_sound_off_is_sent_on_every_member() {
    let mut ingest = profiled(0, 4);
    let out = decoded(&mut ingest, &cc(0, CCNumber::ALL_SOUND_OFF, 0));
    let channels: Vec<u8> = out.iter().map(|(c, _)| *c).collect();
    assert_eq!(channels, [0, 1, 2, 3]);
}

/// **The manager's Pitch Bend is each member channel's own channel bend**
/// (RP-053: the manager's bend moves every note of the zone; a member's own is
/// per-note and a receiver sums the two, M2-104 section 7.4.13): it goes on,
/// then the same bend word on each member channel, and none beyond the zone.
#[test]
fn a_managers_bend_is_copied_onto_each_member_channel() {
    let mut ingest = MpeIngest::new(midi::dual_zones(3, 2));
    let bend = pitch_bend14(0, 12288);
    let Ok(Message::Midi2(_, Voice2::PitchBend(word))) = bend.1.decode() else {
        panic!("a bend");
    };
    let out = decoded(&mut ingest, &bend);
    assert_eq!(
        out,
        (0..=3)
            .map(|c| (c, Voice2::PitchBend(word)))
            .collect::<Vec<_>>()
    );
    // The upper zone's manager bends its own.
    let out = decoded(&mut ingest, &pitch_bend14(15, 100));
    assert_eq!(
        out.iter().map(|(c, _)| *c).collect::<Vec<_>>(),
        [15, 13, 14]
    );
    // A member's own bend is per-note and does not touch the channel bend.
    ingest.translated(&note_on(2, 60, 100));
    let own = decoded(&mut ingest, &pitch_bend14(2, 16383));
    assert!(matches!(
        own.as_slice(),
        [(2, Voice2::PerNotePitchBend { .. })]
    ));
}

/// **A member's Reset All Controllers does not lose the manager's bend it
/// carries**: the reset centres the member's wheel, so the manager's, which has
/// not moved, is sent again after it. With no manager bend there is nothing to
/// send; and a manager's own reset centres the copies too, so a member reset
/// after it sends none.
#[test]
fn a_members_reset_sends_the_managers_bend_again() {
    let mut ingest = MpeIngest::new(midi::lower_zone(3));
    let reset = cc(2, CCNumber::RESET_ALL, 0);
    assert_eq!(ingest.translated(&reset).collect::<Vec<_>>(), vec![reset]);

    let bend = pitch_bend14(0, 12288);
    let Ok(Message::Midi2(_, Voice2::PitchBend(word))) = bend.1.decode() else {
        panic!("a bend");
    };
    ingest.translated(&bend);
    let out: Vec<UmpAt> = ingest.translated(&reset).collect();
    assert_eq!(out[0], reset, "the reset first");
    assert_eq!(out.len(), 2);
    assert_cv2!(out[1], Address { channel, .. }, Voice2::PitchBend(w)
        if channel.get() == 2 && w == word);

    ingest.translated(&cc(0, CCNumber::RESET_ALL, 0));
    assert_eq!(ingest.translated(&reset).collect::<Vec<_>>(), vec![reset]);
}
