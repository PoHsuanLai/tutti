use super::*;
use Speaker::*;

/// The samples come in ascending flag order, so the topology lists the
/// speakers in that order whatever order the flags were written in; the
/// layouts the three codecs state come out as the speakers their specs name.
#[test]
fn flags_become_speakers_in_sample_order() {
    // Vorbis 6: FL FC FR RL RR LFE written, FL FR FC LFE RL RR read.
    let vorbis_six = Channels::FRONT_LEFT
        | Channels::FRONT_CENTRE
        | Channels::FRONT_RIGHT
        | Channels::REAR_LEFT
        | Channels::REAR_RIGHT
        | Channels::LFE1;
    assert_eq!(
        topology_of(vorbis_six).positions(),
        [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight
        ]
    );
    let quad =
        Channels::FRONT_LEFT | Channels::FRONT_RIGHT | Channels::REAR_LEFT | Channels::REAR_RIGHT;
    assert_eq!(
        topology_of(quad).positions(),
        [FrontLeft, FrontRight, BackLeft, BackRight]
    );
    // WAV 5.1 (0x60F): side surrounds.
    let wav_six = Channels::from_bits_truncate(0x60F);
    assert_eq!(
        topology_of(wav_six),
        ChannelTopology::smpte(tutti_types::ChannelLayout::from(6u16)).unwrap()
    );
}

/// A lone channel is the engine's mono, however symphonia names it.
#[test]
fn a_lone_channel_is_mono() {
    assert_eq!(topology_of(Channels::FRONT_LEFT).positions(), [FrontCenter]);
}

/// A flag with no speaker keeps its slot as `Unknown(bit index)`, never
/// `Unassigned` and never dropped: the second LFE is bit 25.
#[test]
fn a_flag_with_no_speaker_keeps_its_slot_as_unknown() {
    let t = topology_of(Channels::LFE1 | Channels::LFE2 | Channels::FRONT_LEFT);
    assert_eq!(t.positions(), [FrontLeft, LowFrequency, Unknown(25)]);
    assert_eq!(
        topology_of(Channels::FRONT_LEFT_WIDE | Channels::FRONT_RIGHT_WIDE).positions(),
        [Unknown(20), Unknown(21)]
    );
}
