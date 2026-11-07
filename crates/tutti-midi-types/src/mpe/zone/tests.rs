use super::*;

#[test]
fn from_mcm_rejects_non_mcm() {
    // A plain note-on is not an MCM.
    assert!(
        MpeZoneConfig::from_mcm(&crate::ump::MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::FIRST,
            60,
            0x8000
        ))
        .is_none()
    );
    // An RPN with a different index is not an MCM.
    let other_rpn = crate::ump::MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        0x00,
        0x00,
        0,
    );
    assert!(MpeZoneConfig::from_mcm(&other_rpn).is_none());
}

/// RP-053's channel layout for every zone shape: which channel is the
/// master, which span the members occupy, and that the two roles never
/// overlap.
///
/// One table rather than a test per constructor: the property is the same
/// sentence in each row, and the rows are what differ. `member_channel_range`
/// is checked against the same span `is_member_channel` reports, so the
/// accessor and the predicate cannot drift apart.
#[test]
fn zone_layout_places_master_and_members_per_rp053() {
    // (config, master channel, inclusive member span)
    let cases = [
        (MpeZoneConfig::lower(10), 0u8, (1u8, 10u8)),
        (MpeZoneConfig::upper(5), 15, (10, 14)),
        // The maximum zone: 15 members leave exactly the master channel out.
        (MpeZoneConfig::lower(15), 0, (1, 15)),
        (MpeZoneConfig::upper(15), 15, (0, 14)),
        // A single-channel (non-MPE) zone has no member channels at all, so
        // the range degenerates to the master's own channel.
        (MpeZoneConfig::single_channel(7), 7, (7, 7)),
    ];

    for (config, master, (first, last)) in cases {
        let label = format!("{:?}", config.zone);
        assert_eq!(config.master_channel, master, "master of {label}");
        assert!(
            config.is_master_channel(master),
            "{label} claims its master"
        );

        let range = config.member_channel_range();
        assert_eq!(
            (*range.start(), *range.end()),
            (first, last),
            "span of {label}"
        );

        if config.member_count == 0 {
            // A single-channel zone's master is not also a member — the
            // range collapsing onto it must not make it one.
            assert!(!config.is_member_channel(master), "{label} has no members");
            continue;
        }

        assert!(config.is_member_channel(first), "{label} first member");
        assert!(config.is_member_channel(last), "{label} last member");
        // The master is never a member, and neither is a channel just
        // outside either end of the span.
        assert!(
            !config.is_member_channel(master),
            "{label} master is not a member"
        );
        assert!(
            !config.is_master_channel(first),
            "{label} first member is not master"
        );
        if let Some(before) = first.checked_sub(1) {
            assert!(!config.is_member_channel(before), "{label} below the span");
        }
        if last < 15 {
            assert!(
                !config.is_member_channel(last + 1),
                "{label} above the span"
            );
        }
    }
}

#[test]
fn test_single_channel_config() {
    let config = MpeZoneConfig::single_channel(5);
    assert_eq!(config.zone, MpeZone::SingleChannel(5));
    assert_eq!(config.master_channel, 5);
    assert_eq!(config.member_count, 0);
    // Standard non-MPE default: ±2, and with no member channels both roles
    // resolve to the same range.
    assert_eq!(
        config.master_pitch_bend_range,
        PitchBendSensitivity::from_semitones(2)
    );
    assert_eq!(
        config.member_pitch_bend_range,
        PitchBendSensitivity::from_semitones(2)
    );

    // Master channel is the single channel
    assert!(config.is_master_channel(5));
    assert!(!config.is_master_channel(0));

    // No member channels in single-channel mode
    assert!(!config.is_member_channel(5));
    assert!(!config.is_member_channel(0));

    // handles_channel: only the master
    assert!(config.handles_channel(5));
    assert!(!config.handles_channel(4));
}

#[test]
fn test_single_channel_clamps_to_15() {
    let config = MpeZoneConfig::single_channel(200);
    assert_eq!(config.master_channel, 15);
    assert_eq!(config.zone, MpeZone::SingleChannel(15));
}

#[test]
fn test_with_pitch_bend_range() {
    // Each role is set independently; setting one leaves the other alone.
    let config = MpeZoneConfig::lower(5).with_master_pitch_bend_range(96);
    assert_eq!(
        config.master_pitch_bend_range,
        PitchBendSensitivity::from_semitones(96)
    );
    assert_eq!(
        config.member_pitch_bend_range,
        PitchBendSensitivity::from_semitones(48),
        "member range untouched"
    );
    assert_eq!(config.member_count, 5); // Other fields unchanged

    let config = MpeZoneConfig::lower(5).with_member_pitch_bend_range(12);
    assert_eq!(
        config.member_pitch_bend_range,
        PitchBendSensitivity::from_semitones(12)
    );
    assert_eq!(
        config.master_pitch_bend_range,
        PitchBendSensitivity::from_semitones(2),
        "master range untouched"
    );
}

/// **A zone starts at MPE's default ranges per role** (RP-053 /
/// M1-100-UM, "Pitch Bend Sensitivity": the master channel ±2 semitones,
/// every member ±48, what a receiver of an MCM resets to). A member at
/// the master's ±2 bends a per-note gesture a 24th as far.
///
/// Mutation (run): swap `MASTER_PITCH_BEND_SEMITONES` and
/// `MEMBER_PITCH_BEND_SEMITONES` back (48 and 2) → fails.
#[test]
fn zones_default_to_rp053_ranges_per_role() {
    for config in [MpeZoneConfig::lower(7), MpeZoneConfig::upper(7)] {
        assert_eq!(
            config.master_pitch_bend_range,
            PitchBendSensitivity::from_semitones(2)
        );
        assert_eq!(
            config.member_pitch_bend_range,
            PitchBendSensitivity::from_semitones(48)
        );

        // …and resolving by channel picks the right one.
        let master = config.master_channel;
        assert_eq!(
            config.pitch_bend_range_for(master),
            config.master_pitch_bend_range
        );
        let member = *config.member_channel_range().start();
        assert_eq!(
            config.pitch_bend_range_for(member),
            config.member_pitch_bend_range,
            "member channel {member} must use the member range"
        );
    }
}

#[test]
fn from_mcm_rejects_out_of_range_configurations() {
    use crate::ump::MidiEvent;
    // RP-053 puts the MCM on Ch1 or Ch16 only; RPN 0x00/0x06 elsewhere is
    // ordinary parameter traffic and must not be read as configuration.
    let on_ch5 = MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(5),
        crate::ump::RPN_BANK_MPE,
        crate::ump::RPN_INDEX_MCM,
        3u32 << 25,
    );
    assert_eq!(MpeZoneConfig::from_mcm(&on_ch5), None);

    // A zone has at most 15 member channels — reject rather than clamp, so a
    // bogus count leaves the existing zone alone.
    let too_many = MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::FIRST,
        crate::ump::RPN_BANK_MPE,
        crate::ump::RPN_INDEX_MCM,
        100u32 << 25,
    );
    assert_eq!(MpeZoneConfig::from_mcm(&too_many), None);

    // The valid case still decodes.
    assert_eq!(
        MpeZoneConfig::from_mcm(&MpeZoneConfig::lower(7).to_mcm()),
        Some((0, 7))
    );
    assert_eq!(
        MpeZoneConfig::from_mcm(&MpeZoneConfig::upper(4).to_mcm()),
        Some((15, 4))
    );
}

#[test]
fn test_member_count_clamped() {
    // Lower zone: member_count clamped to 1..=15
    let config = MpeZoneConfig::lower(0);
    assert_eq!(config.member_count, 1);
    let config = MpeZoneConfig::lower(20);
    assert_eq!(config.member_count, 15);

    // Upper zone: same clamping
    let config = MpeZoneConfig::upper(0);
    assert_eq!(config.member_count, 1);
    let config = MpeZoneConfig::upper(20);
    assert_eq!(config.member_count, 15);
}
