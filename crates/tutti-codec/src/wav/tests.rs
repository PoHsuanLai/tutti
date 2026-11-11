use super::*;
use crate::FormatWriter;
use crate::test_sink::Full;
use std::io::Cursor;
use tutti_types::{ChannelTopology, SampleRate};

fn spec(channels: u16, pcm: Pcm) -> Spec {
    Spec::new(
        crate::Format::Wav(pcm),
        ChannelLayout::from(channels),
        SampleRate(48_000.0),
    )
}

/// `frames` frames of `channels` whose sample `i` is `(i % 64) / 64`: exact in
/// binary, so the quantized values are analytic.
fn ramp(channels: u16, frames: usize) -> Vec<f32> {
    (0..frames * usize::from(channels))
        .map(|i| (i % 64) as f32 / 64.0)
        .collect()
}

fn write_all(spec: &Spec, samples: &[f32]) -> Vec<u8> {
    let mut w = WavWriter::create(spec, wav_pcm(spec), Cursor::new(Vec::new())).unwrap();
    w.write(Interleaved::new(samples, spec.layout)).unwrap();
    w.finish().unwrap().into_inner()
}

fn wav_pcm(spec: &Spec) -> Pcm {
    match spec.format {
        crate::Format::Wav(pcm) => pcm,
        #[allow(unreachable_patterns)]
        _ => unreachable!("a WAV spec"),
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
}

/// The header of a stereo 16-bit file is the 44-byte `PCMWAVEFORMAT` one, field
/// by field, with the sizes back-patched to the frames written.
#[test]
fn a_stereo_16_bit_file_has_the_plain_44_byte_header() {
    let bytes = write_all(&spec(2, Pcm::Int16), &ramp(2, 5));
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(u32_at(&bytes, 4), 36 + 5 * 4, "RIFF size");
    assert_eq!(&bytes[8..16], b"WAVEfmt ");
    assert_eq!(u32_at(&bytes, 16), 16, "fmt chunk size");
    assert_eq!(u16_at(&bytes, 20), 1, "PCM");
    assert_eq!(u16_at(&bytes, 22), 2, "channels");
    assert_eq!(u32_at(&bytes, 24), 48_000, "rate");
    assert_eq!(u32_at(&bytes, 28), 48_000 * 4, "bytes per second");
    assert_eq!(u16_at(&bytes, 32), 4, "block align");
    assert_eq!(u16_at(&bytes, 34), 16, "bits");
    assert_eq!(&bytes[36..40], b"data");
    assert_eq!(u32_at(&bytes, 40), 5 * 4, "data size");
    assert_eq!(bytes.len(), 44 + 5 * 4);
}

/// More than two channels, or more than 16 bits, is `WAVE_FORMAT_EXTENSIBLE`
/// with the container size, valid bits, speaker mask and sub-format.
#[test]
fn wider_or_deeper_files_are_extensible_with_their_mask_and_subformat() {
    for (channels, pcm, mask, float) in [
        (6u16, Pcm::Int24, 0x60Fu32, false),
        (2, Pcm::Int24, 0x3, false),
        (1, Pcm::Float32, 0x4, true),
        (4, Pcm::Int16, 0, false),
    ] {
        let bytes = write_all(&spec(channels, pcm), &ramp(channels, 3));
        let bits = pcm.bits();
        assert_eq!(u32_at(&bytes, 16), 40, "{channels} {pcm:?}: fmt size");
        assert_eq!(u16_at(&bytes, 20), 0xFFFE, "{channels} {pcm:?}: tag");
        assert_eq!(u16_at(&bytes, 22), channels);
        assert_eq!(u16_at(&bytes, 34), bits.div_ceil(8) * 8, "container bits");
        assert_eq!(u16_at(&bytes, 36), 22, "cbSize");
        assert_eq!(u16_at(&bytes, 38), bits, "valid bits");
        assert_eq!(u32_at(&bytes, 40), mask, "{channels} {pcm:?}: mask");
        let guid = if float {
            KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            KSDATAFORMAT_SUBTYPE_PCM
        };
        assert_eq!(&bytes[44..60], &guid, "{channels} {pcm:?}: sub-format");
        assert_eq!(&bytes[60..64], b"data");
        let data = 3 * u32::from(channels) * u32::from(bits.div_ceil(8));
        assert_eq!(u32_at(&bytes, 64), data, "data size");
        assert_eq!(u32_at(&bytes, 4), 60 + data, "RIFF size");
        assert_eq!(bytes.len(), 68 + data as usize);
    }
}

/// An independent reader (`hound`) recovers the quantized ramp at every width
/// and a spread of channel counts: the header and the data agree.
#[test]
fn an_independent_reader_recovers_the_samples_at_every_width() {
    for channels in [1u16, 2, 3, 6] {
        for pcm in [Pcm::Int16, Pcm::Int24, Pcm::Float32] {
            let samples = ramp(channels, 10);
            let bytes = write_all(&spec(channels, pcm), &samples);
            let mut reader = hound::WavReader::new(Cursor::new(bytes)).unwrap();
            assert_eq!(reader.spec().channels, channels);
            assert_eq!(reader.spec().sample_rate, 48_000);
            assert_eq!(reader.spec().bits_per_sample, pcm.bits());
            assert_eq!(reader.len() as usize, samples.len());
            match pcm {
                Pcm::Float32 => {
                    let got: Vec<f32> = reader.samples::<f32>().map(|s| s.unwrap()).collect();
                    assert_eq!(got, samples, "{channels} {pcm:?}");
                }
                _ => {
                    let got: Vec<i32> = reader.samples::<i32>().map(|s| s.unwrap()).collect();
                    let want: Vec<i32> = samples
                        .iter()
                        .map(|&s| match pcm.quantize(s) {
                            Sample::I16(v) => i32::from(v),
                            Sample::I24(v) => v,
                            Sample::F32(_) => unreachable!(),
                        })
                        .collect();
                    assert_eq!(got, want, "{channels} {pcm:?}");
                }
            }
        }
    }
}

/// The topology picks the mask, and one of another width is refused as a spec
/// error before anything is written.
#[test]
fn a_topology_sets_the_mask_and_must_match_the_width() {
    let quad = Spec {
        topology: Some(ChannelTopology::quad()),
        ..spec(4, Pcm::Int24)
    };
    assert_eq!(u32_at(&write_all(&quad, &ramp(4, 2)), 40), 0x33);
    assert_eq!(
        u32_at(&write_all(&spec(4, Pcm::Int24), &ramp(4, 2)), 40),
        0,
        "a bare 4 is unassigned"
    );

    let wrong = Spec {
        topology: Some(ChannelTopology::quad()),
        ..spec(2, Pcm::Int24)
    };
    let mut sink = Cursor::new(Vec::new());
    let err = WavWriter::create(&wrong, Pcm::Int24, &mut sink)
        .expect_err("a quad topology is not stereo");
    assert!(
        matches!(err, Error::Spec(SpecError::TopologyWidth { .. })),
        "{err}"
    );
    assert!(sink.get_ref().is_empty(), "nothing was written");
}

/// A rate no header can hold (zero, not finite, past `u32::MAX` Hz) is refused
/// before the header, rather than saturated into one.
#[test]
fn a_zero_rate_is_refused() {
    for rate in [
        0.0,
        0.4,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        1e10,
        4_294_967_296.0,
    ] {
        let s = Spec {
            rate: SampleRate(rate),
            ..spec(2, Pcm::Int16)
        };
        let err = WavWriter::create(&s, Pcm::Int16, Cursor::new(Vec::new()))
            .err()
            .unwrap_or_else(|| panic!("{rate} Hz was accepted"));
        assert!(
            matches!(err, Error::Spec(SpecError::SampleRate(_))),
            "{rate}: {err}"
        );
    }
}

/// The write that would pass the limit writes the frames that fit, then
/// fails; the running total is exact, so a write that ends exactly at the limit
/// is clean and the next frame is the failure.
#[test]
fn the_limit_stops_at_a_whole_frame_and_a_take_ending_on_it_is_clean() {
    let s = spec(2, Pcm::Int16);
    let mut w = WavWriter::create(&s, Pcm::Int16, Cursor::new(Vec::new())).unwrap();
    w.limit_frames(5);
    let block = ramp(2, 3);
    w.write(Interleaved::new(&block, s.layout)).unwrap();
    w.write(Interleaved::new(&block[..4], s.layout))
        .expect("exactly the limit is clean");
    let err = w
        .write(Interleaved::new(&block[..2], s.layout))
        .expect_err("one frame past");
    assert!(
        matches!(err, Error::WavTooLarge { limit_frames: 5 }),
        "{err}"
    );

    // The block that crosses it is written up to the limit.
    let mut w = WavWriter::create(&s, Pcm::Int16, Cursor::new(Vec::new())).unwrap();
    w.limit_frames(5);
    let eight = ramp(2, 8);
    let err = w
        .write(Interleaved::new(&eight, s.layout))
        .expect_err("8 frames cross a limit of 5");
    assert!(
        matches!(err, Error::WavTooLarge { limit_frames: 5 }),
        "{err}"
    );
    let bytes = w.finish().unwrap().into_inner();
    assert_eq!(u32_at(&bytes, 40), 5 * 4, "five whole frames");
    assert_eq!(bytes.len(), 44 + 5 * 4);
    assert_eq!(
        hound::WavReader::new(Cursor::new(bytes)).unwrap().len(),
        10,
        "the prefix is readable"
    );
}

/// A limit above the format's own is ignored: the limit only comes down.
#[test]
fn the_limit_only_comes_down() {
    let s = spec(2, Pcm::Int16);
    let mut w = WavWriter::create(&s, Pcm::Int16, Cursor::new(Vec::new())).unwrap();
    let real = w.max_frames;
    w.limit_frames(u64::MAX);
    assert_eq!(w.max_frames, real);
}

/// The frame limit is the whole-frame share of the 32-bit data size left after
/// the largest header, and the RIFF size at the limit still fits a `u32`.
///
/// The figures are literal: `u32::MAX - 60` data bytes (4 294 967 235) over the
/// frame size, rounded down. Frame sizes: mono 16 bit 2 bytes, stereo 24 bit 6,
/// six channels of float 24, eight channels of 24 bit 24.
#[test]
fn the_frame_limit_fits_both_32_bit_sizes() {
    for (n, pcm, frames, frame_bytes) in [
        (1u16, Pcm::Int16, 2_147_483_617u64, 2u64),
        (2, Pcm::Int24, 715_827_872, 6),
        (6, Pcm::Float32, 178_956_968, 24),
        (8, Pcm::Int24, 178_956_968, 24),
    ] {
        let layout = ChannelLayout::from(n);
        assert_eq!(max_data_frames(layout, pcm), frames, "{n} {pcm:?}");
        let data = frames * frame_bytes;
        // The largest header is 68 bytes; the RIFF size is the file less 8.
        assert!(data + 60 <= u64::from(u32::MAX), "{n} {pcm:?}: RIFF size");
        assert!(
            data + frame_bytes + 60 > u64::from(u32::MAX),
            "{n} {pcm:?}: one more frame must not fit"
        );
    }
}

/// A trailing partial frame is not written: the file always holds whole frames.
#[test]
fn a_trailing_partial_frame_is_not_written() {
    let s = spec(2, Pcm::Int16);
    let mut w = WavWriter::create(&s, Pcm::Int16, Cursor::new(Vec::new())).unwrap();
    w.write(Interleaved::new(&[0.5, 0.5, 0.5], s.layout))
        .unwrap();
    let bytes = w.finish().unwrap().into_inner();
    assert_eq!(bytes.len(), 44 + 4);
    assert_eq!(u32_at(&bytes, 40), 4);
}

/// A full disk in the middle of a take is an error from the write that hit it,
/// not a short file that reports success, and what landed before it stays
/// readable once the header is back-patched over bytes already there.
///
/// The sink takes the header and 6 whole frames of 4 bytes, no more.
#[test]
fn a_full_disk_mid_take_fails_the_write_that_hit_it() {
    let s = spec(2, Pcm::Int16);
    let mut w = WavWriter::create(&s, Pcm::Int16, Full::holding(44 + 6 * 4)).unwrap();
    w.write(Interleaved::new(&ramp(2, 4), s.layout)).unwrap();
    let err = w
        .write(Interleaved::new(&ramp(2, 6), s.layout))
        .expect_err("two of those frames do not fit");
    match err {
        Error::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::StorageFull),
        other => panic!("expected the sink's error, got {other}"),
    }
    // The frames before the failed write are the file; closing it patches the
    // header over them.
    let sink = w.finish().unwrap();
    let bytes = sink.bytes().to_vec();
    assert_eq!(u32_at(&bytes, 40), 4 * 4, "four frames are counted");
    assert_eq!(hound::WavReader::new(Cursor::new(bytes)).unwrap().len(), 8);
}

/// A WAV writer dropped without `finish` (a pump thread that panicked) still
/// back-patches its sizes, so what was written is a readable file.
///
/// The reader is `hound`, independent of the writer.
#[test]
fn a_dropped_writer_leaves_a_readable_file() {
    let s = spec(2, Pcm::Int16);
    let mut sink = Cursor::new(Vec::new());
    {
        let mut w = WavWriter::create(&s, Pcm::Int16, &mut sink).unwrap();
        w.write(Interleaved::new(&ramp(2, 7), s.layout)).unwrap();
    }
    let reader = hound::WavReader::new(Cursor::new(sink.into_inner())).unwrap();
    assert_eq!(reader.len(), 14);
}

/// A finished writer's drop does not patch twice or touch the returned sink.
#[test]
fn a_finished_writer_hands_back_its_sink_untouched_by_drop() {
    let s = spec(2, Pcm::Int16);
    let mut w = WavWriter::create(&s, Pcm::Int16, Cursor::new(Vec::new())).unwrap();
    w.write(Interleaved::new(&ramp(2, 7), s.layout)).unwrap();
    let bytes = w.finish().unwrap().into_inner();
    assert_eq!(bytes.len(), 44 + 7 * 4);
}
