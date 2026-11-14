//! The bytes each encoder writes for a known input, pinned.
//!
//! The input is analytic and exact in binary (a sawtooth plus a bounded
//! integer hash, scaled by powers of two), so the quantized samples are fixed
//! figures with no libm in them. The digests were taken from the encoders as
//! they stood in `tutti-export` and the live WAV sink before this crate existed, so a
//! change to a header field, a quantization, a FLAC level or the mask shows as a
//! different digest.
//!
//! WAV and AIFF are integer and exact arithmetic and asserted everywhere. FLAC
//! is not pinned: its bytes are the encoder library's choice (and call libm),
//! so its suite holds what the format promises instead, that every level decodes
//! back to the quantized input, and that the levels differ.

#![cfg(any(
    feature = "encode-wav",
    feature = "encode-aiff",
    feature = "encode-flac"
))]

use std::io::Cursor;
use tutti_codec::{Encoder, Format, Spec};
use tutti_types::{ChannelLayout, Interleaved, SampleRate};

fn fnv(bytes: &[u8]) -> u64 {
    let mut h = tutti_fixtures::Fnv::new();
    h.bytes(bytes.iter().copied());
    h.finish()
}

/// `frames` interleaved frames of `channels`: a 1024-step sawtooth offset per
/// channel, plus a 15-bit hash scaled by 1/16, every term exact in `f32`.
fn input(channels: usize, frames: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| {
            (0..channels).map(move |c| {
                let noise = ((i as u64 * i as u64 * 2_654_435_761 + c as u64 * 977) >> 11) % 32_768;
                ((i + c * 13) % 1024) as f32 / 1024.0 - 0.5 + noise as f32 / 32_768.0 / 16.0
            })
        })
        .collect()
}

/// The file `format` writes for `frames` frames of the input, as one block cut
/// in two, and its length and digest.
fn written(format: Format, channels: u16, frames: usize) -> (usize, u64) {
    let layout = ChannelLayout::from(channels);
    let spec = Spec::of_width(format, layout, SampleRate::SR_44K1);
    let samples = input(usize::from(channels), frames);
    let cut = frames / 3 * usize::from(channels);
    let mut encoder = Encoder::create(&spec, Cursor::new(Vec::new())).unwrap();
    encoder
        .write(Interleaved::new(&samples[..cut], layout))
        .unwrap();
    encoder
        .write(Interleaved::new(&samples[cut..], layout))
        .unwrap();
    let bytes = encoder.finish().unwrap().into_inner();
    (bytes.len(), fnv(&bytes))
}

#[cfg(feature = "encode-wav")]
#[test]
fn wav_bytes_are_pinned() {
    use tutti_codec::Pcm::{Float32, Int16, Int24};
    for (pcm, channels, frames, len, digest) in [
        (Int16, 1u16, 10_000usize, 20_044, 0x1ac2_fabb_cb1f_a287u64),
        (Int16, 2, 10_000, 40_044, 0x4c52_8967_01b7_b147),
        (Int16, 6, 10_000, 120_068, 0x9f63_259e_98cd_9126),
        (Int24, 1, 10_000, 30_068, 0x3462_a020_93b3_4973),
        (Int24, 2, 10_000, 60_068, 0xff9b_0e79_369c_9901),
        (Int24, 6, 10_000, 180_068, 0xc7fa_7858_393a_60fd),
        (Float32, 1, 10_000, 40_068, 0x87c0_d7ac_6bac_8c35),
        (Float32, 2, 10_000, 80_068, 0x0fb0_dba7_323e_49c4),
        (Float32, 6, 10_000, 240_068, 0xe287_6b34_1144_01a9),
        (Int24, 2, 3, 86, 0x2d53_d3b0_cbeb_f59d),
        (Int24, 2, 0, 68, 0x5e18_ef2a_1686_adfe),
    ] {
        assert_eq!(
            written(Format::Wav(pcm), channels, frames),
            (len, digest),
            "WAV {pcm:?} {channels} channels {frames} frames"
        );
    }
}

#[cfg(feature = "encode-aiff")]
#[test]
fn aiff_bytes_are_pinned() {
    use tutti_codec::Pcm::{Float32, Int16, Int24};
    for (pcm, channels, frames, len, digest) in [
        (Int16, 2u16, 10_000usize, 40_054, 0x12dc_55f4_ffc1_5e11u64),
        (Int16, 6, 10_000, 120_054, 0x83a9_ef6b_cd2f_5558),
        (Int24, 2, 10_000, 60_054, 0xddc5_828a_ba91_f4ed),
        (Int24, 6, 10_000, 180_054, 0x20a1_212e_ae6e_64d4),
        (Float32, 2, 10_000, 80_072, 0xebc3_d350_b7a6_59bc),
        (Float32, 6, 10_000, 240_072, 0x4884_472c_cd7f_edde),
        (Int24, 2, 3, 72, 0x7968_6240_21a1_9123),
        (Int24, 2, 0, 38, 0x8d8a_467c_7cbd_6d91),
    ] {
        assert_eq!(
            written(Format::Aiff(pcm), channels, frames),
            (len, digest),
            "AIFF {pcm:?} {channels} channels {frames} frames"
        );
    }
}

/// The file `format` writes for the input, decoded back: one `Vec` per channel.
#[cfg(feature = "encode-flac")]
fn decoded(format: Format, channels: u16, frames: usize) -> Vec<Vec<f32>> {
    let layout = ChannelLayout::from(channels);
    let spec = Spec::of_width(format, layout, SampleRate::SR_44K1);
    let samples = input(usize::from(channels), frames);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("written");
    let mut encoder = Encoder::create_file(&spec, &path).unwrap();
    encoder.write(Interleaved::new(&samples, layout)).unwrap();
    encoder.finish().unwrap();
    let wave = tutti_codec::Wave::read(&path).unwrap();
    assert_eq!(
        wave.channels(),
        usize::from(channels),
        "{format:?} channels"
    );
    (0..wave.channels())
        .map(|c| wave.channel(c).to_vec())
        .collect()
}

/// FLAC is lossless: at every level and depth, the file decodes to exactly the
/// samples the WAV writer of the same depth stores, which is the one
/// quantization (and whose bytes are pinned above).
#[cfg(all(feature = "encode-flac", feature = "encode-wav"))]
#[test]
fn flac_decodes_to_the_quantized_input_at_every_level() {
    use tutti_codec::FlacLevel::{Best, Fast, Fastest, Medium, Standard};
    use tutti_codec::IntDepth::{Int16, Int24};
    use tutti_codec::Pcm;
    for (depth, pcm) in [(Int16, Pcm::Int16), (Int24, Pcm::Int24)] {
        for (channels, frames) in [(1u16, 3usize), (2, 10_000), (6, 10_000)] {
            let quantized = decoded(Format::Wav(pcm), channels, frames);
            for level in [Fastest, Fast, Medium, Standard, Best] {
                assert_eq!(
                    decoded(Format::Flac { depth, level }, channels, frames),
                    quantized,
                    "FLAC {level:?} {depth:?} {channels}ch {frames}"
                );
            }
        }
    }
}

/// The five levels code this input with different options, so no level is
/// mapped onto another's: their files differ pairwise.
#[cfg(feature = "encode-flac")]
#[test]
fn the_flac_levels_write_different_files() {
    use tutti_codec::FlacLevel::{Best, Fast, Fastest, Medium, Standard};
    use tutti_codec::IntDepth::Int16;
    let files: Vec<_> = [Fastest, Fast, Medium, Standard, Best]
        .into_iter()
        .map(|level| {
            (
                level,
                written(
                    Format::Flac {
                        depth: Int16,
                        level,
                    },
                    2,
                    10_000,
                ),
            )
        })
        .collect();
    for (i, (a, fa)) in files.iter().enumerate() {
        for (b, fb) in &files[i + 1..] {
            assert_ne!(fa, fb, "FLAC {a:?} and {b:?} wrote the same file");
        }
    }
}
