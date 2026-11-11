use super::*;
use tutti_ump::{NoteNumber, U7, Voice2};

fn ch(n: u8) -> MidiChannel {
    MidiChannel::of(n)
}

fn zone(z: MpeZone, n: u8) -> MpeZoneConfig {
    MpeZoneConfig::new(z, n).expect("a valid member count")
}

fn registered(channel: MidiChannel, rpn: Rpn, value: u32) -> Ump {
    Message::Midi2(
        Address {
            group: MidiGroup::FIRST,
            channel,
        },
        Voice2::Registered {
            bank: rpn.bank(),
            index: rpn.index(),
            value,
        },
    )
    .into()
}

/// RP-053: a zone has 1 to 15 member channels (a count of 0 in an MCM disables
/// the zone and is not a zone). Nothing is clamped.
#[test]
fn a_zone_has_one_to_fifteen_members() {
    for z in [MpeZone::Lower, MpeZone::Upper] {
        assert_eq!(MpeZoneConfig::new(z, 0), None, "0 members is no zone");
        assert_eq!(MpeZoneConfig::new(z, 16), None);
        assert_eq!(MpeZoneConfig::new(z, 255), None);
        for n in 1..=15 {
            assert_eq!(zone(z, n).member_count(), n);
        }
    }
    assert_eq!(
        MpeZoneConfig::lower(7),
        MpeZoneConfig::new(MpeZone::Lower, 7)
    );
    assert_eq!(
        MpeZoneConfig::upper(7),
        MpeZoneConfig::new(MpeZone::Upper, 7)
    );
}

/// RP-053's channel layout for every zone shape: which channel is the master,
/// which span the members occupy, and that the two roles never overlap.
#[test]
fn zone_layout_places_master_and_members_per_rp053() {
    // (zone, members, master channel, inclusive member span)
    let cases = [
        (MpeZone::Lower, 10, 0u8, (1u8, 10u8)),
        (MpeZone::Upper, 5, 15, (10, 14)),
        // The maximum zone: 15 members leave exactly the master channel out.
        (MpeZone::Lower, 15, 0, (1, 15)),
        (MpeZone::Upper, 15, 15, (0, 14)),
        (MpeZone::Lower, 1, 0, (1, 1)),
        (MpeZone::Upper, 1, 15, (14, 14)),
    ];
    for (z, n, master, (first, last)) in cases {
        let config = zone(z, n);
        let label = format!("{z:?} x{n}");
        assert_eq!(config.master_channel(), ch(master), "master of {label}");
        assert!(config.is_master_channel(ch(master)), "{label}");

        let members: Vec<u8> = config.member_channels().map(MidiChannel::get).collect();
        assert_eq!(
            members,
            (first..=last).collect::<Vec<_>>(),
            "span of {label}, ascending"
        );
        // Every channel has exactly the role the span says, and the master is
        // never a member.
        for c in 0..16 {
            let is_member = (first..=last).contains(&c);
            assert_eq!(config.is_member_channel(ch(c)), is_member, "{label} ch {c}");
            assert_eq!(
                config.is_master_channel(ch(c)),
                c == master,
                "{label} ch {c}"
            );
            assert_eq!(
                config.handles_channel(ch(c)),
                is_member || c == master,
                "{label} ch {c}"
            );
        }
    }
}

#[test]
fn the_bend_ranges_are_set_independently() {
    let config = zone(MpeZone::Lower, 5).with_master_pitch_bend_range(PitchBendSensitivity::of(96));
    assert_eq!(
        config.master_pitch_bend_range(),
        PitchBendSensitivity::of(96)
    );
    assert_eq!(
        config.member_pitch_bend_range(),
        PitchBendSensitivity::of(48),
        "member range untouched"
    );
    assert_eq!(config.member_count(), 5);

    let config = zone(MpeZone::Lower, 5).with_member_pitch_bend_range(PitchBendSensitivity::of(12));
    assert_eq!(
        config.member_pitch_bend_range(),
        PitchBendSensitivity::of(12)
    );
    assert_eq!(
        config.master_pitch_bend_range(),
        PitchBendSensitivity::of(2),
        "master range untouched"
    );
}

/// **A zone starts at MPE's default ranges per role** (RP-053 / M1-100-UM,
/// "Pitch Bend Sensitivity": the master channel 2 semitones, every member 48,
/// what a receiver of an MCM resets to).
#[test]
fn zones_default_to_rp053_ranges_per_role() {
    for z in [MpeZone::Lower, MpeZone::Upper] {
        let config = zone(z, 7);
        assert_eq!(
            config.master_pitch_bend_range(),
            PitchBendSensitivity::of(2)
        );
        assert_eq!(
            config.member_pitch_bend_range(),
            PitchBendSensitivity::of(48)
        );
        // Resolving by channel picks the right one.
        assert_eq!(
            config.pitch_bend_range_for(config.master_channel()),
            config.master_pitch_bend_range()
        );
        let member = config.member_channels().next().unwrap();
        assert_eq!(
            config.pitch_bend_range_for(member),
            config.member_pitch_bend_range(),
            "member channel {member:?} must use the member range"
        );
    }
}

#[test]
fn from_mcm_rejects_non_mcm() {
    let note: Ump = Message::Midi2(
        Address {
            group: MidiGroup::FIRST,
            channel: ch(0),
        },
        Voice2::NoteOn {
            note: NoteNumber::of(60),
            velocity: 0x8000,
            attribute: tutti_ump::Attribute::None,
        },
    )
    .into();
    assert_eq!(MpeZoneConfig::from_mcm(&note), None);
    // An RPN with a different address is not an MCM.
    let other = registered(ch(0), Rpn::PITCH_BEND_SENSITIVITY, 3 << 25);
    assert_eq!(MpeZoneConfig::from_mcm(&other), None);
    // A MIDI 1.0 Control Change on the MCM's channel is not one either.
    let cc: Ump = Message::Midi1(
        Address {
            group: MidiGroup::FIRST,
            channel: ch(0),
        },
        tutti_ump::Voice1::ControlChange {
            controller: tutti_ump::CCNumber::RPN_LSB,
            value: U7::of(6),
        },
    )
    .into();
    assert_eq!(MpeZoneConfig::from_mcm(&cc), None);
}

#[test]
fn from_mcm_rejects_out_of_range_configurations() {
    // RP-053 puts the MCM on Ch1 or Ch16 only; RPN 0x00/0x06 elsewhere is
    // ordinary parameter traffic and must not be read as configuration.
    for c in 1..15 {
        let on = registered(ch(c), Rpn::MPE_CONFIGURATION, 3 << 25);
        assert_eq!(MpeZoneConfig::from_mcm(&on), None, "channel {c}");
    }
    // A zone has at most 15 member channels: rejected, not clamped, so a bogus
    // count leaves the existing zone alone.
    let too_many = registered(ch(0), Rpn::MPE_CONFIGURATION, 100 << 25);
    assert_eq!(MpeZoneConfig::from_mcm(&too_many), None);
    let sixteen = registered(ch(15), Rpn::MPE_CONFIGURATION, 16 << 25);
    assert_eq!(MpeZoneConfig::from_mcm(&sixteen), None);
}

#[test]
fn an_mcm_round_trips_and_zero_disables() {
    for z in [MpeZone::Lower, MpeZone::Upper] {
        for n in 1..=15 {
            assert_eq!(MpeZoneConfig::from_mcm(&zone(z, n).to_mcm()), Some((z, n)));
        }
    }
    // Word values from RP-053 / M2-104 section 7.4.7: Registered Controller
    // bank 0 index 6 on the master channel, the count in the top 7 bits.
    assert_eq!(
        zone(MpeZone::Lower, 7).to_mcm(),
        Ump([0x4020_0006, 7 << 25, 0, 0])
    );
    assert_eq!(
        zone(MpeZone::Upper, 4).to_mcm(),
        Ump([0x402F_0006, 4 << 25, 0, 0])
    );
    // Zero members: the zone is disabled, reported as `0` (not a `new`).
    let off = registered(ch(0), Rpn::MPE_CONFIGURATION, 0);
    assert_eq!(MpeZoneConfig::from_mcm(&off), Some((MpeZone::Lower, 0)));
}

/// Sixteen channels less two masters leave 14 to share: the lower zone takes
/// `1..=l` and the upper `15-u..=14`, disjoint exactly when `l + u <= 14`.
#[test]
fn two_zones_may_not_share_a_member_channel() {
    for l in 1..=15u8 {
        for u in 1..=15u8 {
            let zones = MpeZones::new(MpeZoneConfig::lower(l), MpeZoneConfig::upper(u));
            assert_eq!(zones.is_some(), l + u <= 14, "lower {l}, upper {u}");
        }
    }
    // The boundary, spelled out: 8 + 6 fit, 8 + 7 put channel 8 in both.
    assert!(MpeZones::new(MpeZoneConfig::lower(8), MpeZoneConfig::upper(6)).is_some());
    assert!(MpeZones::new(MpeZoneConfig::lower(8), MpeZoneConfig::upper(7)).is_none());
}

#[test]
fn a_zone_must_be_in_its_own_role() {
    let (l, u) = (zone(MpeZone::Lower, 3), zone(MpeZone::Upper, 3));
    assert!(
        MpeZones::new(None, None).is_none(),
        "no zone is not an arrangement"
    );
    assert!(MpeZones::new(Some(l), Some(l)).is_none(), "two lower zones");
    assert!(
        MpeZones::new(Some(u), None).is_none(),
        "an upper zone as the lower"
    );
    assert!(
        MpeZones::new(None, Some(l)).is_none(),
        "a lower zone as the upper"
    );
    assert!(MpeZones::new(Some(l), None).is_some());
    assert!(MpeZones::new(None, Some(u)).is_some());
}

#[test]
fn zones_answer_which_one_owns_a_channel() {
    let (l, u) = (zone(MpeZone::Lower, 5), zone(MpeZone::Upper, 4));
    let zones = MpeZones::new(Some(l), Some(u)).unwrap();
    assert_eq!(zones.lower(), Some(&l));
    assert_eq!(zones.upper(), Some(&u));
    assert_eq!(zones.zones().count(), 2);
    assert_eq!(zones.zone_of(ch(0)), Some(&l));
    assert_eq!(zones.zone_of(ch(5)), Some(&l));
    assert_eq!(zones.zone_of(ch(6)), None, "between the zones");
    assert_eq!(zones.zone_of(ch(11)), Some(&u));
    assert_eq!(zones.zone_of(ch(15)), Some(&u));

    let one = MpeZones::new(Some(l), None).unwrap();
    assert_eq!(one.upper(), None);
    assert_eq!(one.zone_of(ch(15)), None);
}

#[cfg(feature = "serde")]
mod serde_tests {
    use super::*;

    /// **An MPE setup survives being written and read back.**
    #[test]
    fn an_mpe_mode_round_trips_through_a_real_format() {
        let lower = zone(MpeZone::Lower, 10);
        let upper = zone(MpeZone::Upper, 4);
        for mode in [
            MpeMode::Disabled,
            MpeMode::Zones(MpeZones::new(Some(lower), None).unwrap()),
            MpeMode::Zones(MpeZones::new(None, Some(upper)).unwrap()),
            MpeMode::Zones(MpeZones::new(Some(lower), Some(upper)).unwrap()),
            MpeMode::SingleChannelRotation { channel: ch(3) },
        ] {
            let json = serde_json::to_string(&mode).expect("serializes");
            let back: MpeMode = serde_json::from_str(&json).expect("deserializes");
            assert_eq!(back, mode, "round trip changed the mode: {json}");
        }
    }

    /// The two bend ranges keep their exact 7.25 bits and stay distinct.
    #[test]
    fn the_bend_ranges_keep_their_exact_bits() {
        // 2.5 semitones is representable in 7.25 fixed point, not in whole
        // semitones, so a semitone round trip would lose it.
        let fractional = PitchBendSensitivity::from_rpn_bits(
            PitchBendSensitivity::of(2).to_rpn_bits() + (1 << 24),
        );
        let cfg = zone(MpeZone::Lower, 4).with_member_pitch_bend_range(fractional);
        let back: MpeZoneConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(
            back.member_pitch_bend_range().to_rpn_bits(),
            fractional.to_rpn_bits()
        );
        assert_eq!(back, cfg);
        assert_ne!(
            back.master_pitch_bend_range(),
            back.member_pitch_bend_range()
        );
    }

    /// A document is read back through the constructor's check: a member count
    /// RP-053 refuses, or zones that overlap, never become a value.
    #[test]
    fn persisted_values_are_read_back_through_the_check() {
        let json = |count: u8| {
            let mut v = serde_json::to_value(zone(MpeZone::Lower, 3)).unwrap();
            v["member_count"] = count.into();
            v
        };
        assert!(serde_json::from_value::<MpeZoneConfig>(json(3)).is_ok());
        for bad in [0, 16, 200] {
            assert!(
                serde_json::from_value::<MpeZoneConfig>(json(bad)).is_err(),
                "{bad} members"
            );
        }
        // The same zones, written by hand past the constructor.
        let overlapping = serde_json::json!({
            "lower": serde_json::to_value(zone(MpeZone::Lower, 8)).unwrap(),
            "upper": serde_json::to_value(zone(MpeZone::Upper, 7)).unwrap(),
        });
        assert!(serde_json::from_value::<MpeZones>(overlapping).is_err());
        let wrong_role = serde_json::json!({
            "lower": serde_json::to_value(zone(MpeZone::Upper, 2)).unwrap(),
            "upper": null,
        });
        assert!(serde_json::from_value::<MpeZones>(wrong_role).is_err());
        let none = serde_json::json!({ "lower": null, "upper": null });
        assert!(serde_json::from_value::<MpeZones>(none).is_err());
    }
}
