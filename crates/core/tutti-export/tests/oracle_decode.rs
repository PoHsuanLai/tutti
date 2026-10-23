//! Differential test: decode every container this crate writes with `symphonia`
//! and compare against the WAV.
//!
//! # What the oracle is, and why it is independent
//!
//! The oracle is [`symphonia`], a pure-Rust demuxer/decoder family that shares
//! no code with this crate's encoders. `tutti-export` writes WAV with `hound`,
//! FLAC with `flacenc`, AIFF with `aifc` and Ogg with `vorbis_rs`; symphonia
//! reads all four with its own parsers, its own bit readers and its own scale
//! conventions. It has no knowledge of tutti's quantizer, so a disagreement is
//! evidence about the encoder rather than a shared bug reflected back.
//!
//! Worth checking rather than assuming, because `symphonia` *is* already in the
//! lockfile as a production dependency of `tutti-io` (the file decoder): nothing in
//! `tutti-export/src` names it. The encode path and the decode oracle share no
//! code, which is what makes this differential rather than circular. (Being
//! present already is also why this dev-dep is nearly free — see the PR body.)
//!
//! This is the question the existing Rust suite cannot ask. `roundtrip.rs`
//! reads files back with `hound`, which only speaks WAV, so FLAC, AIFF and Ogg
//! are checked by frame count and header alone — never by content. An encoder
//! that wrote a correctly-sized file full of the wrong samples passes every one
//! of those assertions.
//!
//! # Tolerance rationale — and why the lossless cases use integers
//!
//! WAV, FLAC and AIFF at the same bit depth are *lossless*, so the honest claim
//! is bit equality, and **a float tolerance cannot express it**. The first draft
//! of this file compared WAV against FLAC with a `<= 1 LSB` float tolerance and
//! **passed** with the FLAC quantizer mutated back to truncation — the exact
//! historical bug the export README documents. A round-versus-truncate
//! disagreement is *precisely* one LSB (0.7 → 22937 vs 22936), so any tolerance
//! wide enough to absorb a decoder's normalization is also wide enough to
//! swallow the bug. Both sides are therefore quantized back to integers at the
//! file's own depth and those integers are compared exactly.
//!
//! Ogg Vorbis is lossy, so no such claim exists for it. The bound there is
//! spectral and stated in the test: 20 Hz on the recovered tone (a
//! zero-crossing estimate over a windowed slice, far tighter than any codec
//! failure) and 1 dB of level (Vorbis at default quality holds a pure tone to
//! well under that).

#![cfg(all(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use tutti_core::Amplitude;
use tutti_core::Hz;
use tutti_export::{
    render_to_file, AudioFormat, BitDepth, ChannelLayout, Dither, EncodeConfig, ExportConfig, Flac,
    FrozenClock, RenderConfig, RenderGraph,
};
use tutti_graph::GraphBuilder;
use tutti_nodes::testing::{Const, Osc};

const SR: f64 = 44_100.0;
const DUR: f64 = 0.25;

/// `g`, built for an export at [`SR`] — the rate every config here
/// renders at (a graph prepared at another is refused, not re-rated).
fn built(g: GraphBuilder) -> RenderGraph {
    let (editor, executor) = g
        .build(RenderGraph::prepare(tutti_core::SampleRate(SR)))
        .expect("builds");
    RenderGraph::new(editor, executor).expect("built together")
}

/// A graph emitting the constant `level` on both channels.
fn dc_graph(level: f32) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Const::frame(&[level, level]));
    g.pipe_output(id);
    built(g)
}

/// A graph emitting a `freq` Hz sine on both channels.
fn sine_graph(freq: f32) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(
        Osc::sine(Hz(freq))
            .with_amplitude(Amplitude(0.5))
            .with_layout(ChannelLayout::STEREO),
    );
    g.pipe_output(id);
    built(g)
}

fn config(format: AudioFormat, bit_depth: BitDepth) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: tutti_core::SampleRate(SR),
            duration_seconds: DUR,
            ..Default::default()
        },
        encode: EncodeConfig {
            format,
            bit_depth,
            channels: ChannelLayout::STEREO,
        },
        dither: Dither::Off,
        ..Default::default()
    }
}

/// Decode any container symphonia can probe into interleaved f32.
///
/// The oracle. symphonia has no knowledge of tutti's encoders, its quantizer,
/// or its scale convention — which is the whole point.
fn decode(path: &std::path::Path, ext: &str) -> (u32, usize, Vec<f32>) {
    let file = std::fs::File::open(path).expect("open");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(ext);

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .expect("probe format");
    let mut format = probed.format;
    let track = format.default_track().expect("default track").clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .expect("make decoder");

    let rate = track.codec_params.sample_rate.unwrap_or(0);
    let channels = track.codec_params.channels.map(|c| c.count()).unwrap_or(0);

    let mut out: Vec<f32> = Vec::new();
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track.id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(audio) => {
                let spec = *audio.spec();
                let mut buf = SampleBuffer::<f32>::new(audio.capacity() as u64, spec);
                buf.copy_interleaved_ref(audio);
                out.extend_from_slice(buf.samples());
            }
            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
            Err(_) => break,
        }
    }
    (rate, channels, out)
}

/// FFT-free frequency estimate by zero crossings — enough to tell 1 kHz from
/// anything a broken codec would produce, and it needs no extra dependency.
fn dominant_freq(samples: &[f32], channels: usize, rate: f64) -> f64 {
    let mono: Vec<f32> = samples.chunks(channels).map(|f| f[0]).collect();
    // Skip the codec's ramp-in.
    let s = &mono[mono.len() / 4..mono.len() * 3 / 4];
    let mut crossings = 0usize;
    for w in s.windows(2) {
        if w[0] <= 0.0 && w[1] > 0.0 {
            crossings += 1;
        }
    }
    crossings as f64 * rate / s.len() as f64
}

fn rms(samples: &[f32]) -> f64 {
    (samples
        .iter()
        .map(|&s| (s as f64) * (s as f64))
        .sum::<f64>()
        / samples.len() as f64)
        .sqrt()
}

/// The FLAC must hold what the WAV holds.
///
/// This is the exact question the export README says the Rust suite cannot ask,
/// and the one that caught a private FLAC quantizer that truncated where
/// `tutti_core::pcm` rounds.
///
/// Mutation: `flac.rs`'s `f32_to_i32` swapped back to the historical
/// truncating converter (`(clamped * 32767.0) as i32` in place of
/// `tutti_core::pcm::f32_to_i16`) → fails at sample 0 of the Int16 0.7 case,
/// 22936 vs 22935. One LSB, which is the whole point: the float-tolerance
/// draft of this test passed under exactly this mutation.
#[test]
fn flac_and_wav_agree_sample_for_sample() {
    let d = tempfile::tempdir().unwrap();

    for bit_depth in [BitDepth::Int16, BitDepth::Int24] {
        for level in [0.0f32, 0.3, -0.3, 0.7] {
            let wav = d.path().join(format!("a_{bit_depth:?}_{level}.wav"));
            let flac = d.path().join(format!("a_{bit_depth:?}_{level}.flac"));
            render_to_file(
                dc_graph(level),
                &config(AudioFormat::Wav, bit_depth),
                &FrozenClock,
                &wav,
            )
            .unwrap();
            render_to_file(
                dc_graph(level),
                &config(AudioFormat::Flac(Default::default()), bit_depth),
                &FrozenClock,
                &flac,
            )
            .unwrap();

            let (wr, wc, ws) = decode(&wav, "wav");
            let (fr, fc, fs) = decode(&flac, "flac");
            assert_eq!(wr, fr, "{bit_depth:?} @ {level}: sample rate differs");
            assert_eq!(wc, fc, "{bit_depth:?} @ {level}: channel count differs");

            assert_eq!(
                ws.len(),
                fs.len(),
                "{bit_depth:?} @ {level}: length differs"
            );
            let n = ws.len();
            assert!(n > 1000, "{bit_depth:?} @ {level}: too little audio");
            // NOT a float tolerance. Both files are lossless at the same depth,
            // read by the same decoder, so the honest claim is *bit* equality —
            // and a float comparison cannot express it. A round-versus-truncate
            // disagreement is exactly one LSB (0.7 -> 22937 vs 22936, the bug
            // the export README documents), so any tolerance wide enough to
            // absorb the decoder's own normalization is also wide enough to
            // swallow the bug. Quantize both back to integers and compare those.
            let scale = match bit_depth {
                BitDepth::Int16 => 32_767.0f32,
                _ => 8_388_607.0f32,
            };
            let q = |s: f32| (s * scale).round() as i64;
            let bad = (0..n).find(|&i| q(ws[i]) != q(fs[i]));
            assert!(
                bad.is_none(),
                "{bit_depth:?} @ {level}: WAV and FLAC disagree at sample {} \
                 ({} vs {}) — a lossless pair must be bit-identical",
                bad.unwrap(),
                q(ws[bad.unwrap()]),
                q(fs[bad.unwrap()])
            );
        }
    }
}

/// AIFF must hold what the WAV holds — same claim, different container.
///
/// Mutation: scale the AIFF encoder's Int16 sample by 0.999 before quantizing →
/// fails at sample 0 of the 0.3 case, 9830 vs 9820.
#[test]
fn aiff_and_wav_agree_sample_for_sample() {
    let d = tempfile::tempdir().unwrap();

    for bit_depth in [BitDepth::Int16, BitDepth::Int24] {
        for level in [0.3f32, -0.7] {
            let wav = d.path().join(format!("b_{bit_depth:?}_{level}.wav"));
            let aiff = d.path().join(format!("b_{bit_depth:?}_{level}.aiff"));
            render_to_file(
                dc_graph(level),
                &config(AudioFormat::Wav, bit_depth),
                &FrozenClock,
                &wav,
            )
            .unwrap();
            render_to_file(
                dc_graph(level),
                &config(AudioFormat::Aiff, bit_depth),
                &FrozenClock,
                &aiff,
            )
            .unwrap();

            let (_, _, ws) = decode(&wav, "wav");
            let (_, _, as_) = decode(&aiff, "aiff");
            let n = std::cmp::Ord::min(ws.len(), as_.len());
            assert!(n > 1000, "{bit_depth:?} @ {level}: too little audio");
            // Integer comparison, for the reason given in the FLAC case.
            let scale = match bit_depth {
                BitDepth::Int16 => 32_767.0f32,
                _ => 8_388_607.0f32,
            };
            let q = |s: f32| (s * scale).round() as i64;
            let bad = (0..n).find(|&i| q(ws[i]) != q(as_[i]));
            assert!(
                bad.is_none(),
                "{bit_depth:?} @ {level}: WAV and AIFF disagree at sample {} ({} vs {})",
                bad.unwrap(),
                q(ws[bad.unwrap()]),
                q(as_[bad.unwrap()])
            );
        }
    }
}

/// Ogg is lossy, so the claim is spectral: the tone survives at the right
/// frequency and roughly the right level.
///
/// Mutation: halve the samples handed to the Ogg encoder → fails the 1 dB level
/// bound at −6.33 dB (the frequency assertion still passes, which is why the
/// level one has to be here).
#[test]
fn ogg_preserves_the_tone() {
    let d = tempfile::tempdir().unwrap();
    let wav = d.path().join("tone.wav");
    let ogg = d.path().join("tone.ogg");
    render_to_file(
        sine_graph(1000.0),
        &config(AudioFormat::Wav, BitDepth::Float32),
        &FrozenClock,
        &wav,
    )
    .unwrap();
    render_to_file(
        sine_graph(1000.0),
        &config(
            AudioFormat::OggVorbis(Default::default()),
            BitDepth::Float32,
        ),
        &FrozenClock,
        &ogg,
    )
    .unwrap();

    let (wr, wc, ws) = decode(&wav, "wav");
    let (or, oc, os) = decode(&ogg, "ogg");
    assert_eq!(wr, or, "sample rate differs");
    assert_eq!(wc, oc, "channel count differs");

    let wf = dominant_freq(&ws, wc, wr as f64);
    let of = dominant_freq(&os, oc, or as f64);
    // Zero-crossing estimate over a windowed slice: a few Hz of slack, far
    // tighter than any codec failure would be.
    assert!(
        (wf - 1000.0).abs() < 20.0,
        "the WAV control is not 1 kHz ({wf} Hz) — the harness is broken, not the codec"
    );
    assert!(
        (of - wf).abs() < 20.0,
        "Ogg tone came back at {of} Hz, WAV at {wf} Hz"
    );

    // Vorbis at default quality holds level to well under a dB on a pure tone.
    let wl = rms(&ws);
    let ol = rms(&os);
    let db = 20.0 * (ol / wl).log10();
    assert!(
        db.abs() < 1.0,
        "Ogg level moved by {db} dB relative to the WAV (rms {ol} vs {wl})"
    );
}

/// `Flac::compression_level` reaches the encoder: every level decodes to
/// the WAV's samples bit for bit, and level 8 writes a smaller file than
/// level 0 for the same render.
///
/// The render is a sine with both channels equal, so the side channel is
/// silence: level 0 codes left and right independently, and every higher
/// level tries mid/side and codes the silent side in a few bits.
///
/// Mutation (run): ignore the level (`encoder_config` returning
/// `EncoderConfig::default()` for every level) → the two files are the same
/// size and the size assertion fails.
#[test]
fn the_flac_compression_level_changes_the_size_not_the_samples() {
    let d = tempfile::tempdir().unwrap();
    let wav = d.path().join("s.wav");
    render_to_file(
        sine_graph(1_000.0),
        &config(AudioFormat::Wav, BitDepth::Int16),
        &FrozenClock,
        &wav,
    )
    .unwrap();
    let (_, _, ws) = decode(&wav, "wav");
    let q = |s: f32| (s * 32_767.0).round() as i64;

    let mut sizes = Vec::new();
    for level in [0u8, 3, 5, 8] {
        let flac = d.path().join(format!("s{level}.flac"));
        render_to_file(
            sine_graph(1_000.0),
            &config(
                AudioFormat::Flac(Flac {
                    compression_level: level,
                }),
                BitDepth::Int16,
            ),
            &FrozenClock,
            &flac,
        )
        .unwrap();
        let (_, _, fs) = decode(&flac, "flac");
        assert_eq!(fs.len(), ws.len(), "level {level}: length differs");
        assert!(
            ws.iter().zip(&fs).all(|(&a, &b)| q(a) == q(b)),
            "level {level}: the FLAC does not hold the WAV's samples"
        );
        sizes.push(std::fs::metadata(&flac).unwrap().len());
    }
    assert!(
        sizes[3] < sizes[0],
        "level 8 ({} bytes) should be smaller than level 0 ({} bytes)",
        sizes[3],
        sizes[0]
    );
}

/// Decode a FLAC with symphonia's MD5 check on: the interleaved samples, the
/// width, and whether the STREAMINFO MD5 matched what was decoded.
fn decode_flac_verified(path: &std::path::Path) -> (Vec<f32>, usize, Option<bool>) {
    let file = std::fs::File::open(path).expect("open");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("flac");
    let mut format = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .expect("probe format")
        .format;
    let track = format.default_track().expect("default track").clone();
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions { verify: true })
        .expect("make decoder");
    let channels = track.codec_params.channels.map_or(0, |c| c.count());
    let mut out = Vec::new();
    while let Ok(packet) = format.next_packet() {
        let audio = decoder.decode(&packet).expect("a valid frame");
        let mut buf = SampleBuffer::<f32>::new(audio.capacity() as u64, *audio.spec());
        buf.copy_interleaved_ref(audio);
        out.extend_from_slice(buf.samples());
    }
    (out, channels, decoder.finalize().verify_ok)
}

/// **A FLAC decodes to exactly the frames rendered, and its MD5 checks.**
///
/// The render is `tail` frames with no duration, so its length is that
/// figure, analytically. Every length here either fills its last 4096-frame
/// block exactly or leaves a short one, including tails under flacenc's
/// 64-frame block minimum (FLAC allows any length for the last block). The
/// samples are the WAV's, quantized to integers as above, at three widths.
///
/// Mutations (run): encode the last block padded to the full block size
/// (`buf.resize(BLOCK_SIZE)` in `FrameWriter::block`) → every short-tailed
/// length decodes 4096-frame-rounded and the length assertion fails (the
/// pre-streaming encoder's behaviour); hash each sample's full four bytes
/// (`v.to_le_bytes()` without the `bytes_per_sample` cut) → the MD5 check
/// fails on every file.
#[test]
fn a_flac_decodes_to_exactly_the_rendered_frames_with_a_valid_md5() {
    let d = tempfile::tempdir().unwrap();
    for width in [1usize, 2, 6] {
        let layout = ChannelLayout::from(width);
        let graph = || {
            let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout);
            let id = g.add(
                Osc::sine(Hz(997.0))
                    .with_amplitude(Amplitude(0.5))
                    .with_layout(layout),
            );
            g.pipe_output(id);
            built(g)
        };
        for (bit_depth, scale) in [
            (BitDepth::Int16, 32_767.0f32),
            (BitDepth::Int24, 8_388_607.0),
        ] {
            for frames in [1usize, 15, 63, 64, 4095, 4096, 4097, 3 * 4096 + 17] {
                let cfg = |format| ExportConfig {
                    render: RenderConfig {
                        sample_rate: tutti_core::SampleRate(SR),
                        duration_seconds: 0.0,
                        tail: tutti_core::Samples(frames),
                        ..Default::default()
                    },
                    encode: EncodeConfig {
                        format,
                        bit_depth,
                        channels: layout,
                    },
                    dither: Dither::Off,
                    ..Default::default()
                };
                let tag = format!("w{width}_{bit_depth:?}_{frames}");
                let wav = d.path().join(format!("{tag}.wav"));
                render_to_file(graph(), &cfg(AudioFormat::Wav), &FrozenClock, &wav).unwrap();
                let (_, _, ws) = decode(&wav, "wav");
                assert_eq!(
                    ws.len(),
                    frames * width,
                    "{tag}: the WAV is the analytic length"
                );
                for level in [0u8, 8] {
                    let flac = d.path().join(format!("{tag}_{level}.flac"));
                    let format = AudioFormat::Flac(Flac {
                        compression_level: level,
                    });
                    render_to_file(graph(), &cfg(format), &FrozenClock, &flac).unwrap();
                    let (fs, fc, md5_ok) = decode_flac_verified(&flac);
                    assert_eq!(fc, width, "{tag} level {level}: width");
                    assert_eq!(
                        fs.len(),
                        frames * width,
                        "{tag} level {level}: decoded {} frames, rendered {frames}",
                        fs.len() / width
                    );
                    let q = |s: f32| (s * scale).round() as i64;
                    assert!(
                        ws.iter().zip(&fs).all(|(&a, &b)| q(a) == q(b)),
                        "{tag} level {level}: the FLAC does not hold the WAV's samples"
                    );
                    assert_eq!(
                        md5_ok,
                        Some(true),
                        "{tag} level {level}: the MD5 does not match"
                    );
                }
            }
        }
    }
}
