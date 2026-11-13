use super::*;
use tutti_types::SampleRate;
use tutti_types::Seconds;

/// `push_frame` appends to every channel and advances the length once.
#[test]
fn push_frame_appends_one_frame_across_channels() {
    let mut w = Wave::new(2, SampleRate::SR_48K);
    w.push_frame(&[0.25, -0.5]);
    assert_eq!(w.len(), 1);
    assert_eq!((w.at(0, 0), w.at(1, 0)), (0.25, -0.5));
}

#[test]
#[should_panic(expected = "frame width")]
fn push_frame_rejects_a_frame_of_the_wrong_width() {
    Wave::new(2, SampleRate::SR_48K).push_frame(&[0.5]);
}

#[test]
fn zero_rounds_the_duration_to_the_nearest_frame() {
    let w = Wave::zero(1, SampleRate::of(2.0), Seconds::new(0.75));
    assert_eq!(w.len(), 2);
    assert!(w.channel(0).iter().all(|&s| s == 0.0));
}

/// A wave built from a count assumes the engine's order where it has one and
/// says "unassigned" where it does not (4 is quad or B-format; 3 and 5 have no
/// order), and one built from a topology keeps it, one channel per speaker.
#[test]
fn a_count_assumes_smpte_or_unassigned_and_a_topology_is_kept() {
    use tutti_types::{ChannelLayout, ChannelTopology, Speaker};
    for n in [1u16, 2, 6, 8, 12] {
        let w = Wave::new(usize::from(n), SampleRate::SR_48K);
        assert_eq!(
            Some(w.topology()),
            ChannelTopology::smpte(ChannelLayout::from(n)).as_ref(),
            "{n}"
        );
    }
    for n in [3usize, 4, 5, 7, 16] {
        assert_eq!(
            Wave::new(n, SampleRate::SR_48K).topology().positions(),
            vec![Speaker::Unassigned; n],
            "{n}"
        );
    }
    assert!(
        Wave::new(0, SampleRate::SR_48K)
            .topology()
            .positions()
            .is_empty()
    );
    assert_eq!(
        Wave::from_samples(SampleRate::SR_48K, &[0.5])
            .topology()
            .positions(),
        [Speaker::FrontCenter]
    );

    let quad = Wave::with_topology(ChannelTopology::quad(), SampleRate::SR_48K);
    assert_eq!(quad.channels(), 4);
    assert_eq!(quad.topology(), &ChannelTopology::quad());
}

/// A wave wider than a layout holds is refused, not given the wrapped width
/// (65 538 channels would otherwise be a stereo topology over 65 538 vectors).
#[test]
#[should_panic(expected = "at most u16::MAX channels")]
fn a_wave_wider_than_a_layout_is_refused() {
    let _ = Wave::new(usize::from(u16::MAX) + 1, SampleRate::SR_48K);
}

/// `frames` frames over `topology` whose sample at `(c, i)` is
/// `((i * 7 + c * 3) % 400 - 200) / 256`: a ramp per channel that wraps, and
/// exact at 16 bits (a whole multiple of 128 / 32768).
#[cfg(all(feature = "encode-wav", feature = "encode-flac", decode))]
fn ramp_wave(topology: ChannelTopology, rate: f64, frames: usize) -> Wave {
    let mut wave = Wave::with_topology(topology, SampleRate::of(rate));
    let width = wave.channels();
    for i in 0..frames {
        let frame: Vec<f32> = (0..width)
            .map(|c| (((i * 7 + c * 3) % 400) as f32 - 200.0) / 256.0)
            .collect();
        wave.push_frame(&frame);
    }
    wave
}

/// Writing a wave and reading the file back gives the same samples, rate and
/// topology: exactly for WAV's float format, and to within one 16-bit step
/// (the writer scales by 32767 and the reader by 32768) for the integer ones.
#[cfg(all(feature = "encode-wav", feature = "encode-flac", decode))]
#[test]
fn a_written_wave_reads_back_as_itself() {
    use crate::{FlacLevel, Format, IntDepth, Pcm};
    let dir = tempfile::tempdir().unwrap();
    let cases = [
        (
            "stereo.wav",
            Format::Wav(Pcm::Int16),
            ChannelTopology::smpte(ChannelLayout::STEREO).unwrap(),
        ),
        (
            "float.wav",
            Format::Wav(Pcm::Float32),
            ChannelTopology::smpte(ChannelLayout::STEREO).unwrap(),
        ),
        ("quad.wav", Format::Wav(Pcm::Int16), ChannelTopology::quad()),
        (
            "stereo.flac",
            Format::Flac {
                depth: IntDepth::Int16,
                level: FlacLevel::Standard,
            },
            ChannelTopology::smpte(ChannelLayout::STEREO).unwrap(),
        ),
    ];
    for (name, format, topology) in cases {
        // Past one write chunk, so the chunking is exercised too.
        let wave = ramp_wave(topology, 44_100.0, 20_000);
        let path = dir.path().join(name);
        wave.write(&path, format).unwrap();
        let back = Wave::read(&path).unwrap();
        assert_eq!(back.sample_rate(), wave.sample_rate(), "{name}: rate");
        assert_eq!(back.topology(), wave.topology(), "{name}: topology");
        assert_eq!(back.len(), wave.len(), "{name}: length");
        let step = if format == Format::Wav(Pcm::Float32) {
            0.0
        } else {
            1.0 / 32_768.0
        };
        for c in 0..wave.channels() {
            for (i, (&got, &want)) in back.channel(c).iter().zip(wave.channel(c)).enumerate() {
                assert!(
                    (got - want).abs() <= step,
                    "{name}: channel {c}, frame {i}: {got} for {want}"
                );
            }
        }
    }
}

/// `interleaved` is frame-major: the order of an `Interleaved` block.
#[test]
fn interleaved_is_frame_major() {
    let mut w = Wave::new(2, SampleRate::SR_48K);
    w.push_frame(&[1.0, 2.0]);
    w.push_frame(&[3.0, 4.0]);
    assert_eq!(w.interleaved().collect::<Vec<_>>(), [1.0, 2.0, 3.0, 4.0]);
    assert_eq!(w.layout(), ChannelLayout::STEREO);
}
