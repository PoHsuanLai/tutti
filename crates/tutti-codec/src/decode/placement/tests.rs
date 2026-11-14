use super::*;
use Speaker::*;

/// The samples come in ascending flag order, so the topology lists the
/// speakers in that order whatever order the flags were written in; the
/// layouts the three codecs state come out as the speakers their specs name.
#[test]
fn flags_become_speakers_in_sample_order() {
    // Vorbis 6: FL FC FR RL RR LFE written, FL FR FC LFE RL RR read.
    let vorbis_six = Channels::Positioned(
        Position::FRONT_LEFT
            | Position::FRONT_CENTER
            | Position::FRONT_RIGHT
            | Position::REAR_LEFT
            | Position::REAR_RIGHT
            | Position::LFE1,
    );
    assert_eq!(
        topology_of(&vorbis_six).positions(),
        [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight
        ]
    );
    let quad = Channels::Positioned(
        Position::FRONT_LEFT | Position::FRONT_RIGHT | Position::REAR_LEFT | Position::REAR_RIGHT,
    );
    assert_eq!(
        topology_of(&quad).positions(),
        [FrontLeft, FrontRight, BackLeft, BackRight]
    );
    // WAV 5.1 (0x60F): side surrounds.
    let wav_six = Channels::Positioned(Position::from_bits_truncate(0x60F));
    assert_eq!(
        topology_of(&wav_six),
        ChannelTopology::smpte(tutti_types::ChannelLayout::from(6u16)).unwrap()
    );
}

/// A lone channel is the engine's mono, however symphonia names it.
#[test]
fn a_lone_channel_is_mono() {
    assert_eq!(
        topology_of(&Channels::Positioned(Position::FRONT_LEFT)).positions(),
        [FrontCenter]
    );
    assert_eq!(
        topology_of(&Channels::Discrete(1)).positions(),
        [FrontCenter]
    );
}

/// A flag with no speaker keeps its slot as `Unknown(bit index)`, never
/// `Unassigned` and never dropped: the second LFE is bit 18.
#[test]
fn a_flag_with_no_speaker_keeps_its_slot_as_unknown() {
    let t = topology_of(&Channels::Positioned(
        Position::LFE1 | Position::LFE2 | Position::FRONT_LEFT,
    ));
    assert_eq!(t.positions(), [FrontLeft, LowFrequency, Unknown(18)]);
    assert_eq!(
        topology_of(&Channels::Positioned(
            Position::FRONT_LEFT_WIDE | Position::FRONT_RIGHT_WIDE
        ))
        .positions(),
        [Unknown(24), Unknown(25)]
    );
}

/// Channels that name no position feed no speaker: each keeps its slot as
/// `Unassigned`.
#[test]
fn channels_without_positions_are_unassigned() {
    assert_eq!(
        topology_of(&Channels::Discrete(3)).positions(),
        [Unassigned; 3]
    );
}
