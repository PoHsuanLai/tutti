use super::*;
use tutti_ump::{NoteNumber, NoteVelocity, U7, Voice2};

const MASTER: PitchBendSensitivity = PitchBendSensitivity::MPE_MASTER_DEFAULT;
const MEMBER: PitchBendSensitivity = PitchBendSensitivity::MPE_MEMBER_DEFAULT;

fn ch(n: u8) -> MidiChannel {
    MidiChannel::of(n)
}

fn zone(z: MpeZone, n: u8) -> MpeZoneConfig {
    ranged(
        z,
        n,
        PitchBendSensitivity::MPE_MASTER_DEFAULT,
        PitchBendSensitivity::MPE_MEMBER_DEFAULT,
    )
}

fn ranged(
    z: MpeZone,
    n: u8,
    master: PitchBendSensitivity,
    member: PitchBendSensitivity,
) -> MpeZoneConfig {
    MpeZoneConfig::new(z, n, master, member).expect("a valid member count")
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
            value: ControlValue::new(value),
        },
    )
    .into()
}

/// RP-053: a zone has 1 to 15 member channels (a count of 0 in an MCM disables
/// the zone and is not a zone). Nothing is clamped.
#[test]
fn a_zone_has_one_to_fifteen_members() {
    for z in [MpeZone::Lower, MpeZone::Upper] {
        assert_eq!(
            MpeZoneConfig::new(z, 0, MASTER, MEMBER),
            None,
            "0 members is no zone"
        );
        assert_eq!(MpeZoneConfig::new(z, 16, MASTER, MEMBER), None);
        assert_eq!(MpeZoneConfig::new(z, 255, MASTER, MEMBER), None);
        for n in 1..=15 {
            assert_eq!(zone(z, n).member_count(), n);
        }
    }
    assert_eq!(
        MpeZoneConfig::lower(
            7,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT
        ),
        MpeZoneConfig::new(MpeZone::Lower, 7, MASTER, MEMBER)
    );
    assert_eq!(
        MpeZoneConfig::upper(
            7,
            PitchBendSensitivity::MPE_MASTER_DEFAULT,
            PitchBendSensitivity::MPE_MEMBER_DEFAULT
        ),
        MpeZoneConfig::new(MpeZone::Upper, 7, MASTER, MEMBER)
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
    let config = ranged(MpeZone::Lower, 5, PitchBendSensitivity::of(96), MEMBER);
    assert_eq!(
        config.pitch_bend_range_for(config.master_channel()),
        PitchBendSensitivity::of(96)
    );
    assert_eq!(
        config.pitch_bend_range_for(ch(1)),
        PitchBendSensitivity::of(48),
        "member range untouched"
    );
    assert_eq!(config.member_count(), 5);

    let config = ranged(MpeZone::Lower, 5, MASTER, PitchBendSensitivity::of(12));
    assert_eq!(
        config.pitch_bend_range_for(ch(1)),
        PitchBendSensitivity::of(12)
    );
    assert_eq!(
        config.pitch_bend_range_for(config.master_channel()),
        PitchBendSensitivity::of(2),
        "master range untouched"
    );
}

/// **A zone starts at MPE's default ranges per role** (RP-053 / M1-100-UM,
/// Pitch Bend Sensitivity: the master channel 2 semitones, every member 48,
/// what a receiver of an MCM resets to).
#[test]
fn zones_default_to_rp053_ranges_per_role() {
    for z in [MpeZone::Lower, MpeZone::Upper] {
        let config = zone(z, 7);
        assert_eq!(
            config.pitch_bend_range_for(config.master_channel()),
            PitchBendSensitivity::of(2)
        );
        // Resolving by channel picks the right one for every member.
        for member in config.member_channels() {
            assert_eq!(
                config.pitch_bend_range_for(member),
                PitchBendSensitivity::of(48),
                "member channel {member:?} must use the member range"
            );
        }
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
            velocity: NoteVelocity::new(0x8000),
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
            let zones = MpeZones::new(
                MpeZoneConfig::lower(
                    l,
                    PitchBendSensitivity::MPE_MASTER_DEFAULT,
                    PitchBendSensitivity::MPE_MEMBER_DEFAULT,
                ),
                MpeZoneConfig::upper(
                    u,
                    PitchBendSensitivity::MPE_MASTER_DEFAULT,
                    PitchBendSensitivity::MPE_MEMBER_DEFAULT,
                ),
            );
            assert_eq!(zones.is_some(), l + u <= 14, "lower {l}, upper {u}");
        }
    }
    // The boundary, spelled out: 8 + 6 fit, 8 + 7 put channel 8 in both.
    assert!(
        MpeZones::new(
            MpeZoneConfig::lower(
                8,
                PitchBendSensitivity::MPE_MASTER_DEFAULT,
                PitchBendSensitivity::MPE_MEMBER_DEFAULT
            ),
            MpeZoneConfig::upper(
                6,
                PitchBendSensitivity::MPE_MASTER_DEFAULT,
                PitchBendSensitivity::MPE_MEMBER_DEFAULT
            )
        )
        .is_some()
    );
    assert!(
        MpeZones::new(
            MpeZoneConfig::lower(
                8,
                PitchBendSensitivity::MPE_MASTER_DEFAULT,
                PitchBendSensitivity::MPE_MEMBER_DEFAULT
            ),
            MpeZoneConfig::upper(
                7,
                PitchBendSensitivity::MPE_MASTER_DEFAULT,
                PitchBendSensitivity::MPE_MEMBER_DEFAULT
            )
        )
        .is_none()
    );
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

fn pzone(manager: u8, channels: u8) -> MpeZoneConfig {
    MpeZoneConfig::profile(
        ch(manager),
        channels,
        PitchBendSensitivity::MPE_PROFILE_DEFAULT,
    )
    .expect("a layout that fits")
}

/// M2-120 section 3.5: a Profile starts at 48 for the Manager and for every
/// Member, where MPE 1.1 starts the Manager at 2.
#[test]
fn the_profile_defaults_the_manager_to_48_and_mpe_1_1_to_2() {
    assert_eq!(
        PitchBendSensitivity::MPE_PROFILE_DEFAULT,
        PitchBendSensitivity::of(48)
    );
    assert_eq!(
        PitchBendSensitivity::MPE_MEMBER_DEFAULT,
        PitchBendSensitivity::of(48)
    );
    assert_eq!(
        PitchBendSensitivity::MPE_MASTER_DEFAULT,
        PitchBendSensitivity::of(2)
    );
}

/// M2-120 section 3.5: the Manager and every Member share one range, so a
/// Profile zone answers the same for its Manager and for a Member, whatever
/// range it was built with.
#[test]
fn a_profile_zone_answers_one_range_for_its_manager_and_a_member() {
    for range in [
        PitchBendSensitivity::MPE_PROFILE_DEFAULT,
        PitchBendSensitivity::of(24),
    ] {
        let z = MpeZoneConfig::profile(ch(3), 5, range).unwrap();
        for c in MidiChannel::all() {
            assert_eq!(z.pitch_bend_range_for(c), range, "channel {c:?}");
        }
    }
}

/// The constructor records the specification: MPE 1.1 zones follow MPE 1.1 and
/// a Profile zone the Profile, and only an MPE 1.1 zone has an anchor side.
#[test]
fn a_zone_follows_the_specification_it_was_built_for() {
    assert_eq!(zone(MpeZone::Lower, 3).spec(), MpeSpec::Mpe11);
    assert_eq!(zone(MpeZone::Upper, 3).spec(), MpeSpec::Mpe11);
    assert_eq!(pzone(0, 4).spec(), MpeSpec::Profile);
    assert_eq!(zone(MpeZone::Upper, 3).zone(), Some(MpeZone::Upper));
    assert_eq!(pzone(0, 4).zone(), None);
}

/// M2-120 appendix A: the Number of Channels counts the Manager and the
/// Members, and a zone is a Manager with at least one Member that fits in the
/// sixteen channels.
#[test]
fn a_profile_zone_is_a_manager_and_at_least_one_member_within_sixteen_channels() {
    assert_eq!(
        MpeZoneConfig::profile(ch(0), 0, PitchBendSensitivity::MPE_PROFILE_DEFAULT),
        None
    );
    assert_eq!(
        MpeZoneConfig::profile(ch(0), 1, PitchBendSensitivity::MPE_PROFILE_DEFAULT),
        None,
        "a Manager alone"
    );
    assert_eq!(
        MpeZoneConfig::profile(ch(0), 17, PitchBendSensitivity::MPE_PROFILE_DEFAULT),
        None
    );
    assert_eq!(
        MpeZoneConfig::profile(ch(0), 255, PitchBendSensitivity::MPE_PROFILE_DEFAULT),
        None
    );
    assert_eq!(
        MpeZoneConfig::profile(ch(14), 3, PitchBendSensitivity::MPE_PROFILE_DEFAULT),
        None,
        "runs past channel 16"
    );
    assert_eq!(
        MpeZoneConfig::profile(ch(15), 2, PitchBendSensitivity::MPE_PROFILE_DEFAULT),
        None
    );
    assert!(MpeZoneConfig::profile(ch(0), 16, PitchBendSensitivity::MPE_PROFILE_DEFAULT).is_some());
    assert!(MpeZoneConfig::profile(ch(14), 2, PitchBendSensitivity::MPE_PROFILE_DEFAULT).is_some());
    for manager in 0..16u8 {
        for channels in 0..=255u8 {
            let fits = channels >= 2 && u16::from(manager) + u16::from(channels) <= 16;
            assert_eq!(
                MpeZoneConfig::profile(
                    ch(manager),
                    channels,
                    PitchBendSensitivity::MPE_PROFILE_DEFAULT
                )
                .is_some(),
                fits,
                "manager {manager}, {channels} channels"
            );
        }
    }
}

/// M2-120 section 2.2: the Manager is the zone's lowest channel, and the
/// Members are the channels above it, on any Manager.
#[test]
fn the_manager_is_the_lowest_channel_and_the_members_follow_it() {
    let z = pzone(4, 5);
    assert_eq!(z.master_channel(), ch(4));
    assert_eq!(z.channel_count(), 5);
    assert_eq!(z.member_count(), 4);
    assert_eq!(
        z.member_channels()
            .map(MidiChannel::get)
            .collect::<Vec<_>>(),
        [5, 6, 7, 8]
    );
    for c in MidiChannel::all() {
        let (manager, member) = (c.get() == 4, (5..=8).contains(&c.get()));
        assert_eq!(z.is_master_channel(c), manager, "channel {c:?}");
        assert_eq!(z.is_member_channel(c), member, "channel {c:?}");
        assert_eq!(z.handles_channel(c), manager || member, "channel {c:?}");
    }
}

/// M2-120 section 3.4: zones never share a channel; a set refuses one that does,
/// and is the same set whichever order its zones were added.
#[test]
fn profile_zones_never_share_a_channel_and_a_set_has_no_order() {
    let low = pzone(0, 4);
    let mid = pzone(4, 4);
    let high = pzone(8, 8);
    assert_eq!(MpeZones::single(low).with(pzone(3, 2)), None);
    assert_eq!(MpeZones::single(low).with(low), None);
    assert_eq!(MpeZones::single(high).with(pzone(7, 2)), None);
    let a = MpeZones::single(low)
        .with(mid)
        .and_then(|s| s.with(high))
        .expect("disjoint zones");
    let b = MpeZones::single(high)
        .with(low)
        .and_then(|s| s.with(mid))
        .expect("disjoint zones");
    assert_eq!(a, b);
    assert_eq!(
        a.zones()
            .map(|z| z.master_channel().get())
            .collect::<Vec<_>>(),
        [0, 4, 8]
    );
    assert_eq!(a.zone_of(ch(6)), Some(&mid));
    assert_eq!(a.zone_of(ch(8)), Some(&high));
}

/// Two channels is the least a Profile zone has, so eight zones fill the sixteen
/// channels and a ninth cannot be added.
#[test]
fn eight_profile_zones_fill_the_channels_and_a_ninth_is_refused() {
    let mut set = MpeZones::single(pzone(0, 2));
    for manager in (2..16).step_by(2) {
        set = set.with(pzone(manager, 2)).expect("fits");
    }
    assert_eq!(set.zones().count(), MpeZones::MAX_ZONES);
    assert!(MidiChannel::all().all(|c| set.zone_of(c).is_some()));
    for manager in 0..15 {
        assert_eq!(set.with(pzone(manager, 2)), None, "manager {manager}");
    }
}

/// MPE 1.1 has a lower and an upper zone: a second of either shares the
/// anchor's channel, and a third zone has nowhere to go.
#[test]
fn mpe_1_1_has_at_most_one_lower_and_one_upper_zone() {
    let (l, u) = (zone(MpeZone::Lower, 3), zone(MpeZone::Upper, 3));
    let both = MpeZones::single(l).with(u).expect("a lower and an upper");
    assert_eq!(both.zones().count(), 2);
    for count in 1..=15 {
        assert_eq!(
            both.with(zone(MpeZone::Lower, count)),
            None,
            "lower {count}"
        );
        assert_eq!(
            both.with(zone(MpeZone::Upper, count)),
            None,
            "upper {count}"
        );
    }
    assert_eq!(MpeZones::single(l).with(l), None, "a second lower zone");
    assert_eq!(MpeZones::single(u).with(u), None, "a second upper zone");
}

/// A set follows one specification: a Profile zone is refused beside an MPE 1.1
/// zone even on channels the 1.1 zone does not use, and the other way round.
#[test]
fn a_set_does_not_mix_specifications() {
    let lower = zone(MpeZone::Lower, 3);
    assert_eq!(MpeZones::single(lower).with(pzone(8, 4)), None);
    assert_eq!(MpeZones::single(pzone(8, 4)).with(lower), None);
}

/// Taking a zone out is by its master; the last one out leaves no set.
#[test]
fn removing_the_last_zone_leaves_no_set() {
    let both = MpeZones::single(pzone(0, 4)).with(pzone(8, 4)).unwrap();
    assert_eq!(both.without(ch(0)), Some(MpeZones::single(pzone(8, 4))));
    assert_eq!(both.without(ch(5)), Some(both), "no zone is managed there");
    assert_eq!(both.without(ch(0)).and_then(|s| s.without(ch(8))), None);
}

/// Both specifications keep a response for the members: the Profile's Channel
/// Response, and the mode an MPE 1.1 zone starts in.
#[test]
fn a_zone_keeps_its_channel_response() {
    for z in [pzone(0, 4), zone(MpeZone::Lower, 3)] {
        assert_eq!(z.channel_response(), ChannelResponse::Polyphonic);
        assert_eq!(
            z.with_channel_response(ChannelResponse::Monophonic)
                .channel_response(),
            ChannelResponse::Monophonic
        );
    }
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
        let cfg = ranged(MpeZone::Lower, 4, MASTER, fractional);
        let back: MpeZoneConfig =
            serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
        let (master, member) = (ch(0), ch(1));
        assert_eq!(
            back.pitch_bend_range_for(member).to_rpn_bits(),
            fractional.to_rpn_bits()
        );
        assert_eq!(back, cfg);
        assert_ne!(
            back.pitch_bend_range_for(master),
            back.pitch_bend_range_for(member)
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
        let zones = |zones: &[MpeZoneConfig]| serde_json::to_value(zones).unwrap();
        let overlapping = zones(&[zone(MpeZone::Lower, 8), zone(MpeZone::Upper, 7)]);
        assert!(serde_json::from_value::<MpeZones>(overlapping).is_err());
        let mixed = zones(&[zone(MpeZone::Lower, 2), pzone(8, 4)]);
        assert!(serde_json::from_value::<MpeZones>(mixed).is_err());
        let none = zones(&[]);
        assert!(serde_json::from_value::<MpeZones>(none).is_err());
    }

    /// A persisted Profile zone and set are read back through the check too.
    #[test]
    fn a_persisted_profile_zone_is_read_back_through_its_check() {
        let set = MpeZones::single(pzone(0, 4).with_channel_response(ChannelResponse::Monophonic))
            .with(MpeZoneConfig::profile(ch(8), 8, PitchBendSensitivity::of(24)).unwrap())
            .unwrap();
        let json = serde_json::to_string(&set).unwrap();
        assert_eq!(serde_json::from_str::<MpeZones>(&json).unwrap(), set);

        let mut bad = serde_json::to_value(pzone(0, 4)).unwrap();
        bad["member_count"] = 0.into();
        assert!(serde_json::from_value::<MpeZoneConfig>(bad).is_err());
        let mut bad = serde_json::to_value(pzone(0, 4)).unwrap();
        bad["placement"] = serde_json::to_value(Placement::Managed {
            manager: ch(15),
            range: PitchBendSensitivity::MPE_PROFILE_DEFAULT,
        })
        .unwrap();
        assert!(serde_json::from_value::<MpeZoneConfig>(bad).is_err());
    }
}
