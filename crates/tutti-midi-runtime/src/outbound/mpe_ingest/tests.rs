use super::*;
use tutti_midi_types::convert::{
    midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2,
};
use tutti_midi_types::mpe::MpeZoneConfig;

fn note_on(channel: u8, note: u8, vel_u7: u8) -> MidiEvent {
    MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        note,
        midi1_velocity_to_midi2(vel_u7),
    )
}
fn pitch_bend14(channel: u8, bend14: u16) -> MidiEvent {
    MidiEvent::pitch_bend(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        midi1_pitch_bend_to_midi2(bend14),
    )
}
fn cc(channel: u8, cc_num: CCNumber, value_u7: u8) -> MidiEvent {
    MidiEvent::cc(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        cc_num,
        midi1_cc_to_midi2(value_u7),
    )
}

/// True if the event is the given native CV2 kind — matched inline so the
/// borrow of `data_words()` never escapes.
macro_rules! assert_cv2 {
    ($ev:expr, $pat:pat $(if $guard:expr)?) => {
        match UmpMessage::try_from($ev.data_words()).unwrap() {
            UmpMessage::ChannelVoice2($pat) $(if $guard)? => {}
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
            .translate(&note_on(2, 60, 100))
            .next()
            .unwrap()
            .is_note_on()
    );
    // Channel bend on ch2 → per-note bend on note 60.
    let out = ingest
        .translate(&pitch_bend14(2, 16383))
        .next()
        .expect("emits");
    assert_cv2!(
        out,
        ChannelVoice2::PerNotePitchBend(m)
            if u8::from(m.channel()) == 2 && u8::from(m.note_number()) == 60
    );
}

#[test]
fn master_channel_bend_passes_through_as_channel_bend() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    // Master channel of a lower zone is channel 0.
    let out = ingest
        .translate(&pitch_bend14(0, 12288))
        .next()
        .expect("passes through");
    assert_cv2!(out, ChannelVoice2::ChannelPitchBend(_));
}

#[test]
fn member_channel_cc74_becomes_per_note_controller() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    ingest.translate(&note_on(3, 64, 100));
    let out = ingest
        .translate(&cc(3, CCNumber::BRIGHTNESS, 127))
        .next()
        .expect("emits");
    assert_cv2!(
        out,
        ChannelVoice2::AssignablePerNoteController(m)
            if u8::from(m.note_number()) == 64 && m.index() == 74
    );
}

#[test]
fn member_channel_pressure_becomes_per_note_pressure() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    ingest.translate(&note_on(2, 60, 100));
    let out = ingest
        .translate(&MidiEvent::channel_pressure(
            MidiGroup::FIRST,
            MidiChannel::of(2),
            0xFFFF_FFFF,
        ))
        .next()
        .expect("emits");
    assert_cv2!(out, ChannelVoice2::KeyPressure(m) if u8::from(m.note_number()) == 60);
}

#[test]
fn bend_with_no_held_note_is_dropped() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    // No note on ch2 yet → member-channel bend has nothing to address.
    assert!(ingest.translate(&pitch_bend14(2, 16383)).next().is_none());
}

#[test]
fn native_per_note_passes_through() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
    let native =
        MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF_FFFF);
    let out = ingest.translate(&native).next().expect("passes");
    assert_cv2!(out, ChannelVoice2::PerNotePitchBend(_));
}

#[test]
fn disabled_mode_passes_everything_through() {
    let mut ingest = MpeIngest::new(MpeMode::Disabled);
    let out = ingest
        .translate(&pitch_bend14(2, 16383))
        .next()
        .expect("passes");
    assert_cv2!(out, ChannelVoice2::ChannelPitchBend(_));
}

#[test]
fn rotation_ignores_off_channel() {
    let mut ingest = MpeIngest::new(MpeMode::SingleChannelRotation { channel: 5 });
    // On-channel note passes.
    assert!(ingest.translate(&note_on(5, 60, 100)).next().is_some());
    // Off-channel note is ignored (this mode owns channel 5).
    assert!(ingest.translate(&note_on(7, 64, 100)).next().is_none());
}

#[test]
fn mcm_from_the_wire_configures_the_lower_zone() {
    // Start disabled; an MCM on the master channel (0) declaring 7 members
    // must enable the lower zone from the wire (M2-104 §7.4.7).
    let mut ingest = MpeIngest::new(MpeMode::Disabled);
    let mcm = MpeZoneConfig::lower(7).to_mcm();

    // The MCM itself is absorbed (configuration, not a musical event);
    // what goes on is the zone's reset ranges, all registered controllers
    // (`an_mcm_resets_its_zones_bend_ranges`).
    let reset: Vec<MidiEvent> = ingest.translate(&mcm).collect();
    assert_eq!(reset.len(), 8, "master and seven members");
    assert!(!reset.contains(&mcm));
    assert!(matches!(ingest.mode(), MpeMode::LowerZone(_)));

    // Now a member-channel bend folds to per-note, proving the zone is live.
    ingest.translate(&note_on(1, 60, 100));
    let out = ingest
        .translate(&pitch_bend14(1, 16383))
        .next()
        .expect("emits");
    assert_cv2!(out, ChannelVoice2::PerNotePitchBend(m) if u8::from(m.note_number()) == 60);
}

#[test]
fn mcm_with_zero_members_disables_the_zone() {
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(7)));
    // A raw MCM on master channel 0 with member_count == 0 disables the lower
    // zone. (`MpeZoneConfig::lower` clamps to >= 1, so the disabling value can
    // only come off the wire — build it directly.)
    let disable = MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::FIRST, // lower-zone master channel
        tutti_midi_types::ump::RPN_BANK_MPE,
        tutti_midi_types::ump::RPN_INDEX_MCM,
        0, // 0 members → disable
    );
    assert!(ingest.translate(&disable).next().is_none());
    assert!(matches!(ingest.mode(), MpeMode::Disabled));
}

/// What a receiver following RPN #00/00 (channel bend range) and
/// #00/07 (per-note bend range) per channel ends with after `events`:
/// `(channel, index) → semitones`, in order of arrival.
fn ranges_after(events: impl IntoIterator<Item = MidiEvent>) -> Vec<(u8, u8, f32)> {
    events
        .into_iter()
        .map(|ev| {
            let Ok(UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m))) =
                UmpMessage::try_from(ev.data_words())
            else {
                panic!("{ev:?} is not a registered controller");
            };
            assert_eq!(u8::from(m.bank()), RPN_BANK_MPE);
            let range = PitchBendSensitivity::from_rpn_bits(m.controller_data());
            (
                u8::from(m.channel()),
                u8::from(m.index()),
                range.as_semitones_f32(),
            )
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
    use tutti_midi_types::PerNoteBendRanges;
    let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(3)));
    let mut receiver = PerNoteBendRanges::default();
    for ev in ingest.translate(&bend_sensitivity(1, 12)) {
        receiver.observe(&ev);
    }
    let mcm = MpeZoneConfig::upper(3).to_mcm().with_frame_offset(9);
    let reset: Vec<MidiEvent> = ingest.translate(&mcm).collect();
    assert!(matches!(ingest.mode(), MpeMode::DualZone { .. }));
    assert!(reset.iter().all(|ev| ev.frame_offset == 9));
    assert_eq!(
        ranges_after(reset.iter().copied()),
        vec![(15, 0, 2.0), (12, 7, 48.0), (13, 7, 48.0), (14, 7, 48.0)]
    );
    // An upper member set to ±12, then the upper MCM again: the upper
    // members are back at ±48, the lower zone's ±12 stands.
    let mcm = MpeZoneConfig::upper(3).to_mcm();
    for ev in ingest.translate(&bend_sensitivity(13, 12)) {
        receiver.observe(&ev);
    }
    assert_eq!(receiver.semitones(13), 12.0);
    for ev in ingest.translate(&mcm) {
        receiver.observe(&ev);
    }
    for channel in 12..=14 {
        assert_eq!(receiver.semitones(channel), 48.0, "channel {channel}");
    }
    assert_eq!(
        receiver.semitones(1),
        12.0,
        "the lower zone keeps its range"
    );

    let disable = MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(15),
        RPN_BANK_MPE,
        tutti_midi_types::ump::RPN_INDEX_MCM,
        0,
    );
    assert_eq!(ingest.translate(&disable).len(), 0);

    // A host-set dual-zone mode: each zone's ranges, lower first.
    ingest.set_mode(MpeMode::DualZone {
        lower: MpeZoneConfig::lower(2),
        upper: MpeZoneConfig::upper(1),
    });
    let defaults = ingest.zone_defaults(MidiGroup::FIRST, 0);
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
    assert_eq!(ingest.zone_defaults(MidiGroup::FIRST, 0).len(), 0);
}

fn bend_sensitivity(channel: u8, semitones: u8) -> MidiEvent {
    MidiEvent::registered_controller(
        MidiGroup::FIRST,
        MidiChannel::of(channel),
        tutti_midi_types::ump::RPN_BANK_MPE,
        RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY,
        tutti_midi_types::PitchBendSensitivity::from_semitones(semitones).to_rpn_bits(),
    )
    .with_frame_offset(17)
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
    use tutti_midi_types::PerNoteBendRanges;
    let mut ingest = MpeIngest::new(MpeMode::DualZone {
        lower: MpeZoneConfig::lower(3),
        upper: MpeZoneConfig::upper(3),
    });
    let mut receiver = PerNoteBendRanges::default();

    let master = bend_sensitivity(0, 2);
    let out: Vec<MidiEvent> = ingest.translate(&master).collect();
    assert_eq!(out, vec![master], "the master's own range passes through");
    assert!(!receiver.observe(&out[0]));

    for (member, semitones, channels) in [(2, 12, 1..=3), (13, 24, 12..=14)] {
        let fanned = ingest.translate(&bend_sensitivity(member, semitones));
        assert_eq!(fanned.len(), 3);
        let mut seen = Vec::new();
        for ev in fanned {
            assert_eq!(ev.frame_offset, 17);
            assert!(receiver.observe(&ev), "{ev:?} is RPN #00/07");
            seen.push(ev.channel().map(MidiChannel::get));
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
