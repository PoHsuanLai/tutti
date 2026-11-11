//! The speaker each decoded channel feeds, from what a container states.
//!
//! The expected topologies are the specs' own: `ksmedia.h`'s `SPEAKER_*` bits
//! for WAVE (the mask is hand-written into the header here, not produced by
//! our writer), the Vorbis I and FLAC channel tables for those.

use std::path::PathBuf;

use tutti_codec::{FileIn, Wave};
use tutti_types::{ChannelLayout, ChannelTopology, Speaker::*};

fn asset(name: &str) -> PathBuf {
    tutti_fixtures::asset(&format!("audio/{name}"))
}

/// A float32 `WAVE_FORMAT_EXTENSIBLE` file whose `n`th channel holds the
/// constant `n + 1` (times 0.01) for 4 frames, declaring `mask`.
fn extensible(channels: u16, mask: u32) -> Vec<u8> {
    let frames = 4u32;
    let mut fmt = Vec::new();
    fmt.extend(0xFFFEu16.to_le_bytes());
    fmt.extend(channels.to_le_bytes());
    fmt.extend(48_000u32.to_le_bytes());
    fmt.extend((48_000 * 4 * u32::from(channels)).to_le_bytes());
    fmt.extend((4 * channels).to_le_bytes());
    fmt.extend(32u16.to_le_bytes());
    fmt.extend(22u16.to_le_bytes());
    fmt.extend(32u16.to_le_bytes());
    fmt.extend(mask.to_le_bytes());
    // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    fmt.extend([
        0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b,
        0x71,
    ]);
    let mut data = Vec::new();
    for _ in 0..frames {
        for c in 0..channels {
            data.extend((0.01 * f32::from(c + 1)).to_le_bytes());
        }
    }
    let mut out = b"RIFF".to_vec();
    out.extend((4 + 8 + fmt.len() as u32 + 8 + data.len() as u32).to_le_bytes());
    out.extend(b"WAVEfmt ");
    out.extend((fmt.len() as u32).to_le_bytes());
    out.extend(&fmt);
    out.extend(b"data");
    out.extend((data.len() as u32).to_le_bytes());
    out.extend(&data);
    out
}

fn topology_of_mask(channels: u16, mask: u32) -> ChannelTopology {
    Wave::from_bytes(extensible(channels, mask))
        .expect("decodes")
        .topology()
        .clone()
}

/// The mask a WAVE header states is the topology, speaker for speaker: quad
/// `0x33` is back, `0x603` side, 5.1 `0x3F` back and `0x60F` side (the engine's
/// own), and the channels stay in file order.
#[test]
fn a_wave_mask_names_each_channels_speaker() {
    assert_eq!(
        topology_of_mask(4, 0x33).positions(),
        [FrontLeft, FrontRight, BackLeft, BackRight]
    );
    assert_eq!(
        topology_of_mask(4, 0x603).positions(),
        [FrontLeft, FrontRight, SideLeft, SideRight]
    );
    assert_eq!(
        topology_of_mask(6, 0x3F).positions(),
        [
            FrontLeft,
            FrontRight,
            FrontCenter,
            LowFrequency,
            BackLeft,
            BackRight
        ]
    );
    assert_eq!(
        topology_of_mask(6, 0x60F),
        ChannelTopology::smpte(ChannelLayout::from(6u16)).unwrap()
    );
    assert_eq!(
        topology_of_mask(3, 0x7).positions(),
        [FrontLeft, FrontRight, FrontCenter]
    );
}

/// A speaker the vocabulary cannot name keeps its channel, as
/// `Unknown(bit index)`: bit 25 is symphonia's second LFE, and the file's
/// channels are not renumbered around it.
#[test]
fn a_speaker_with_no_name_keeps_its_channel() {
    let wave = Wave::from_bytes(extensible(2, 0x0200_0001)).expect("decodes");
    assert_eq!(wave.topology().positions(), [FrontLeft, Unknown(25)]);
    assert_eq!(wave.channels(), 2);
    assert!((wave.at(1, 0) - 0.02).abs() < 1e-6);
}

/// Sample channel `n` is topology entry `n`: the LFE of a 5.1 file is the
/// fourth plane, whatever order the speakers were declared in.
#[test]
fn channel_n_of_the_samples_is_entry_n_of_the_topology() {
    let wave = Wave::from_bytes(extensible(6, 0x3F)).expect("decodes");
    let at = wave.topology().index_of(LowFrequency).unwrap();
    assert_eq!(at, 3);
    assert!((wave.at(at, 0) - 0.04).abs() < 1e-6);
    let centre = wave.topology().index_of(FrontCenter).unwrap();
    assert!((wave.at(centre, 0) - 0.03).abs() < 1e-6);
}

/// The fixtures: a lone channel is the centre (symphonia calls it
/// `FRONT_LEFT`), two are left and right in every container, and the 5.1 file
/// states the back pair. A stream reports what a load does.
#[test]
fn the_fixtures_state_their_speakers() {
    for (name, expected) in [
        ("mono_48k.wav", vec![FrontCenter]),
        ("stereo.flac", vec![FrontLeft, FrontRight]),
        ("stereo.mp3", vec![FrontLeft, FrontRight]),
        ("stereo.ogg", vec![FrontLeft, FrontRight]),
        ("stereo_s16.wav", vec![FrontLeft, FrontRight]),
        (
            "surround_51.wav",
            vec![
                FrontLeft,
                FrontRight,
                FrontCenter,
                LowFrequency,
                BackLeft,
                BackRight,
            ],
        ),
    ] {
        let path = asset(name);
        let wave = Wave::read(&path).expect(name);
        assert_eq!(wave.topology().positions(), expected, "{name}");
        assert_eq!(wave.channels(), expected.len(), "{name}");
        let stream = FileIn::open(&path).expect(name);
        assert_eq!(stream.topology(), wave.topology(), "{name}: stream");
    }
}

/// Ogg Vorbis states its speakers by count and writes its planes in the Vorbis
/// I order (`FL FC FR RL RR LFE` at six); the decoder hands them in the
/// topology's order, so the plane of each speaker is where the topology says.
/// Encoded here straight through `vorbis_rs`, one tone per Vorbis-order
/// plane, so the figures are the spec's table and not our writer's.
#[cfg(all(feature = "encode-vorbis", feature = "decode-ogg"))]
#[test]
fn a_vorbis_six_decodes_in_the_topologys_order() {
    use std::num::{NonZeroU8, NonZeroU32};
    use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoderBuilder};

    // Vorbis I section 4.3.9: 6 channels are front left, front centre, front
    // right, rear left, rear right, LFE.
    let vorbis_order = [
        FrontLeft,
        FrontCenter,
        FrontRight,
        BackLeft,
        BackRight,
        LowFrequency,
    ];
    // One pure tone per plane, each a different frequency (so the identity of a
    // plane survives the codec's gain, which treats the LFE differently), the
    // LFE lowest since the encoder low-passes it.
    let hz =
        |s: tutti_types::Speaker| 150 - 20 * vorbis_order.iter().position(|&x| x == s).unwrap();
    let rate = 44_100usize;
    let planes: Vec<Vec<f32>> = vorbis_order
        .iter()
        .map(|&s| {
            (0..rate)
                .map(|i| {
                    0.5 * (i as f32 * std::f32::consts::TAU * hz(s) as f32 / rate as f32).sin()
                })
                .collect()
        })
        .collect();
    let mut out = Vec::new();
    let mut enc = VorbisEncoderBuilder::new(
        NonZeroU32::new(rate as u32).unwrap(),
        NonZeroU8::new(6).unwrap(),
        &mut out,
    )
    .unwrap()
    .bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
        target_quality: 0.9,
    })
    .build()
    .unwrap();
    enc.encode_audio_block(&planes).unwrap();
    enc.finish().unwrap();

    let wave = Wave::from_bytes(out).expect("decodes");
    assert_eq!(wave.channels(), 6);
    // Rising zero crossings in the middle half second: `hz / 2` of them.
    let crossings = |c: usize| {
        let x = &wave.channel(c)[rate / 4..rate * 3 / 4];
        x.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count()
    };
    for (c, &speaker) in wave.topology().positions().iter().enumerate() {
        let expected = hz(speaker) / 2;
        assert!(
            crossings(c).abs_diff(expected) <= 1,
            "channel {c} ({speaker:?}): {} crossings, want {expected}",
            crossings(c)
        );
    }
}
