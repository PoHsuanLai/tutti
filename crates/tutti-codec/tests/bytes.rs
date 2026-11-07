//! The bytes each encoder writes for a known input, pinned.
//!
//! The input is analytic and exact in binary (a sawtooth plus a bounded
//! integer hash, scaled by powers of two), so the quantized samples are fixed
//! figures with no libm in them. The digests were taken from the encoders as
//! they stood in `tutti-export` and `tutti-io` before this crate existed, so a
//! change to a header field, a quantization, a FLAC level or the mask shows as a
//! different digest.
//!
//! WAV and AIFF are integer and exact arithmetic and asserted everywhere. FLAC
//! is asserted through `assert_golden`, on the target the figures were recorded
//! on, because its encoder calls libm (the window's cosine, the Rice parameter's
//! logarithm).
//!
//! Mutations (each run, each fails the rows named, then reverted): writing the
//! extensible header for `bits() >= 16` fails the stereo 16-bit WAV rows;
//! `Pcm::quantize` truncating instead of rounding fails the integer WAV rows;
//! a zero speaker mask fails the multichannel WAV rows; `FlacLevel::Fastest`
//! given the side-stereo modes, or `Medium` the `Standard` LPC order, fails the
//! FLAC rows of those levels.

#![cfg(any(feature = "wav", feature = "aiff", feature = "flac"))]

use std::io::Cursor;
use tutti_codec::{Encoder, Format, Spec};
use tutti_types::{ChannelLayout, Interleaved, SampleRate};

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
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
    let spec = Spec::new(format, layout, SampleRate(44_100.0));
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

#[cfg(feature = "wav")]
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

#[cfg(feature = "aiff")]
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

/// The five FLAC levels that differ, as the libFLAC-ish levels they replace:
/// 0, 1 and 2, 3, 4 to 6, 7 and 8. Their digests differ on this input, so a
/// level mapped to another's coding options shows.
#[cfg(feature = "flac")]
#[test]
fn flac_bytes_are_pinned_at_every_level() {
    use tutti_codec::FlacLevel::{Best, Fast, Fastest, Medium, Standard};
    use tutti_codec::IntDepth::{Int16, Int24};
    // (level, depth, channels, frames, digest)
    for (level, depth, channels, frames, digest) in [
        (Fastest, Int16, 2u16, 10_000usize, 0xe06e_af14_e0be_7037u64),
        (Fast, Int16, 2, 10_000, 0x7521_4d0d_dc41_93f9),
        (Medium, Int16, 2, 10_000, 0x8365_73b7_024f_6725),
        (Standard, Int16, 2, 10_000, 0xa471_70e5_dfa1_101a),
        (Best, Int16, 2, 10_000, 0x0146_e259_f2aa_01ca),
        (Fastest, Int16, 6, 10_000, 0x2550_5523_58ce_2e63),
        (Best, Int16, 6, 10_000, 0x93ce_97d3_76f6_2bba),
        (Standard, Int24, 1, 3, 0xe374_c1fe_f602_d961),
        (Standard, Int24, 2, 3, 0x4efd_2790_718a_3b4f),
        (Standard, Int24, 2, 0, 0x304f_3d17_993a_f031),
    ] {
        let (_, got) = written(Format::Flac { depth, level }, channels, frames);
        tutti_nodes::testing::assert_golden(
            &format!("FLAC {level:?} {depth:?} {channels}ch {frames}"),
            got,
            digest,
        );
    }
}
