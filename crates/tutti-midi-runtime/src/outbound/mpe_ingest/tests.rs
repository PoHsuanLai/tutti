use super::*;
use tutti_fixtures::midi;
use tutti_graph::UmpAt;
use tutti_midi_protocol::MpeZoneConfig;
use tutti_types::Samples;
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
    let mcm = at(0, MpeZoneConfig::lower(7).unwrap().to_mcm());

    // The MCM itself is absorbed (configuration, not a musical event);
    // what goes on is the zone's reset ranges, all registered controllers
    // (`an_mcm_resets_its_zones_bend_ranges`).
    let reset: Vec<UmpAt> = ingest.translated(&mcm).collect();
    assert_eq!(
        ranges_only(reset.iter().copied()).len(),
        8,
        "master and seven members"
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
    let overlapping = at(0, MpeZoneConfig::upper(5).unwrap().to_mcm());
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
    let fits = at(0, MpeZoneConfig::upper(4).unwrap().to_mcm());
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

/// The Registered Controllers among `events`, the zone's ranges, without the
/// stops (All Sound Off, Reset All Controllers) that go with a layout change.
fn ranges_only(events: impl IntoIterator<Item = UmpAt>) -> Vec<UmpAt> {
    events.into_iter().filter(|ev| !is_stop(ev)).collect()
}

/// Whether `ev` is an All Sound Off or a Reset All Controllers.
fn is_stop(ev: &UmpAt) -> bool {
    matches!(
        tutti_ump::decode(ev.1),
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
            let kind = |ev: &UmpAt| match tutti_ump::decode(ev.1) {
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
/// RP-053 / M1-100-UM, "Pitch Bend Sensitivity": the master channel to
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
    let mcm = at(9, MpeZoneConfig::upper(3).unwrap().to_mcm());
    let reset: Vec<UmpAt> = ingest.translated(&mcm).collect();
    assert!(matches!(
        ingest.mode_of(MidiGroup::FIRST),
        MpeMode::Zones(_)
    ));
    assert!(reset.iter().all(|ev| ev.0.get() == 9));
    assert_eq!(
        ranges_after(ranges_only(reset.iter().copied())),
        vec![(15, 0, 2.0), (12, 7, 48.0), (13, 7, 48.0), (14, 7, 48.0)]
    );
    // An upper member set to ±12, then the upper MCM again: the upper
    // members are back at ±48, the lower zone's ±12 stands.
    let mcm = at(0, MpeZoneConfig::upper(3).unwrap().to_mcm());
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
/// other (M2-104 Appendix B.1: what a message does in one Group "does not
/// apply to Channels in any other Group"), and its reset ranges go out on
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
        8,
        "master and seven members"
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
        .map(|ev| match tutti_ump::decode(ev.1) {
            Ok(Message::Midi2(address, voice)) => (address.channel.get(), voice),
            other => panic!("unexpected message: {other:?}"),
        })
        .collect()
}

/// **A bend received before the Note On is the new note's** (M1-100 section
/// 2.2.6: "The pitch of a new note is affected by the Pitch Bend message most
/// recently received on both the Manager Channel and that note's Member
/// Channel before Note On. A receiver shall continue to track Pitch Bend
/// messages from both the Manager Channel and the Member Channels even when no
/// note is playing"; section 2.4, Table 4: the sender's Pitch Bend comes
/// first). The ingest remembers the bend of the member channel with no note,
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
            if note.get() == 60 && value == tutti_fixtures::midi::wide_bend(16383)
    ));
    assert!(matches!(out[1], (2, Voice2::NoteOn { .. })));
}

/// **Channel Pressure and Control Change #74 received before the Note On are
/// the new note's** (M1-100 section 2.2.7: "The control of a new note is
/// affected by the Channel Pressure message most recently received on its
/// Channel before Note On. A receiver shall continue to track Channel Pressure
/// messages even when no note is playing"; section 2.2.8 the same of Control
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
        (3, Voice2::PolyPressure { note, value }) if note.get() == 64 && value == 0x4000_0000
    ));
    assert!(matches!(
        out[1],
        (3, Voice2::AssignablePerNote { note, controller: 74, value })
            if note.get() == 64 && value == tutti_fixtures::midi::wide_value(64)
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
/// 2.2.6: "A Released Note shall cease to be affected by Pitch Bend messages
/// from the Member Channels after the Note Off message occurs"; section 2.4:
/// "the MPE control messages shall not affect a note after the Note Off
/// message has been received"), while the channel goes on tracking them for
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
/// (M1-100 section 2.2.7: "All MPE receivers shall respond to Channel Pressure
/// both on the Manager Channel and on each Member Channel. If a Device
/// receives Channel Pressure on both ... it shall combine such data
/// meaningfully and separately for each Sounding Note"; section 2.2.8 the same
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
        (1, Voice2::PolyPressure { note, value }) if note.get() == 60 && value == 0x2000_0000
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
            Voice2::PolyPressure { note, value } => Some((*c, note.get(), *value)),
            _ => None,
        })
        .collect();
    assert_eq!(heard, [(1, 60, 0x4000_0000), (2, 64, 0x7000_0000)]);
    assert!(
        matches!(out.last(), Some((0, Voice2::ChannelPressure(0x4000_0000)))),
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
/// on a message that is not for the channel it came on: "it should ignore
/// them"; Table 5, "P: Prohibited"). On the Manager Channel, All Sound Off
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
        let out = ingest
            .translated(&cc(channel, number(controller), 0))
            .count();
        assert_eq!(out == 1, passes, "CC {controller} on channel {channel}");
    }
}

/// **The MPE Configuration Message is not for a Member Channel** (Table 5:
/// RPN #6 on Member Channels, "P"): on a member that is not a master's channel
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
/// (M1-100 section 2.3.3: "A receiver operating in MIDI Mode 3 shall ignore
/// Program Change messages received on Member Channels"; Table 5, Bank Select
/// on Member Channels "Prohibited in MIDI Mode 3. Optional in MIDI Mode 4";
/// section 2.2.4.3: "send the appropriate Mode Message to the lowest numbered
/// Member Channel of a zone (not to the Manager Channel)"). Mono Mode On on
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

/// **A channel in no zone is for conventional use** (M1-100 section 2.2.1:
/// "Any MIDI Channels not assigned to any Zone remain available for
/// conventional use."): notes, bends, pressure and CC 74 there pass as they
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
/// 2.2.3: "when a receiver changes its Zone configurations, the receiver shall
/// stop all Sounding Notes and reset all controls to reasonable default values
/// on each Channel entering or leaving MPE control"). Growing a lower zone from
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

    let grow = at(0, MpeZoneConfig::lower(5).unwrap().to_mcm());
    let out: Vec<UmpAt> = ingest.translated(&grow).collect();
    assert_eq!(stopped(out.iter().copied()), [4, 5]);
    assert_eq!(ranges_only(out).len(), 6, "and the zone's ranges");

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
/// (section 2.2.3, "reset all controls ... on each Channel entering or leaving
/// MPE control"): shrink a zone past a channel that held a note and had been
/// bent, grow it back, and the channel starts clean.
#[test]
fn a_channel_that_left_the_zone_forgets_its_note_and_its_bend() {
    let regrown = || {
        let mut ingest = MpeIngest::new(tutti_fixtures::midi::lower_zone(5));
        ingest.translated(&note_on(4, 62, 100));
        ingest.translated(&pitch_bend14(4, 9000));
        ingest.translated(&at(0, MpeZoneConfig::lower(3).unwrap().to_mcm()));
        ingest.translated(&at(0, MpeZoneConfig::lower(5).unwrap().to_mcm()));
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
/// notes are forgotten (section 2.2.3, "entering or leaving MPE control").
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
