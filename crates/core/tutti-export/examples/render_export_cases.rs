//! Render a matrix of export cases to files, for independent analysis.
//!
//! The companion to `verify_export.py`, and the export-side counterpart of
//! `tutti-sampler`'s `examples/render_cases.rs`. Same division of labour: this
//! half only *produces* audio and encodes the case parameters in the filename;
//! the Python half re-derives what each file should contain from that name and
//! first principles, and judges it. Nothing here asserts anything, so this
//! program cannot make a broken export look correct.
//!
//! # Why a second opinion, when `tests/` already round-trips
//!
//! The Rust tests read files back with `hound`, which only speaks WAV — so FLAC,
//! AIFF and Ogg are checked by frame count and header, never by content. Python's
//! `soundfile` decodes all four uniformly, which is the only practical way to ask
//! "does the FLAC hold the same samples as the WAV". And the resampler's actual
//! quality — passband flatness, aliasing rejection — needs a spectral analysis
//! that a hand-rolled DFT in a test would be reimplementing badly.
//!
//! Run: `cargo run --release -p tutti-export --example render_export_cases -- <outdir>`

use tutti_core::dsp::*;
use tutti_export::{
    render_to_file, AudioFormat, BitDepth, ChannelLayout, Dither, EncodeConfig, ExportConfig,
    RenderConfig, Resample,
};

const SR: f64 = 48_000.0;
const DUR: f64 = 1.0;
/// The reference tone. Chosen to sit well inside every rate under test, so a
/// resampled case cannot alias it into a different bin and look correct.
const TONE_HZ: f32 = 1_000.0;

/// A steady tone at −6 dBFS, in stereo.
///
/// The amplitude is deliberately not full scale: a resampler's passband ripple
/// and an encoder's dither both push samples slightly past their input value, and
/// clipping at the rail would mask that as a flat top rather than reporting it.
fn tone_net() -> tutti_core::dsp::Net {
    let mut n = tutti_core::dsp::Net::new(0, 2);
    let id = n.push(Box::new((sine_hz::<f32>(TONE_HZ) | sine_hz::<f32>(TONE_HZ)) * 0.5));
    n.pipe_output(id);
    n
}

/// Constant DC — the case whose correct output is knowable exactly.
fn dc_net(level: f32) -> tutti_core::dsp::Net {
    let mut n = tutti_core::dsp::Net::new(0, 2);
    let id = n.push(Box::new(dc((level, level))));
    n.pipe_output(id);
    n
}

/// A mono graph, for the upmix/fold cases.
fn mono_net() -> tutti_core::dsp::Net {
    let mut n = tutti_core::dsp::Net::new(0, 1);
    let id = n.push(Box::new(sine_hz::<f32>(TONE_HZ) * 0.5));
    n.pipe_output(id);
    n
}

/// A tone near Nyquist for the downsampling case.
///
/// At 48 k this is a clean 18 kHz. Resampled to 22.05 k its true frequency is
/// above the new Nyquist, so a correct resampler must *filter it out*, and one
/// with no anti-alias filter will fold it down to an audible ~4 kHz. That
/// difference is unmissable in a spectrum and invisible to a frame count.
fn near_nyquist_net() -> tutti_core::dsp::Net {
    let mut n = tutti_core::dsp::Net::new(0, 2);
    let id = n.push(Box::new((sine_hz::<f32>(18_000.0) | sine_hz::<f32>(18_000.0)) * 0.5));
    n.pipe_output(id);
    n
}

fn base(format: AudioFormat, bit_depth: BitDepth, channels: ChannelLayout) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: tutti_core::SampleRate(SR),
            duration_seconds: DUR,
            ..Default::default()
        },
        encode: EncodeConfig {
            format,
            bit_depth,
            channels,
        },
        dither: Dither::Off,
        ..Default::default()
    }
}

fn main() -> tutti_export::Result<()> {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| std::env::temp_dir().to_string_lossy().into_owned());
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir)?;

    let mut count = 0usize;
    let mut write =
        |name: String, net: tutti_core::dsp::Net, cfg: &ExportConfig| -> tutti_export::Result<()> {
            let w = render_to_file(net, cfg, &tutti_export::FrozenClock, &dir.join(&name))?;
            println!("wrote {name} ({} bytes)", w.bytes);
            count += 1;
            Ok(())
        };

    // ---- the control -----------------------------------------------------
    //
    // An unprocessed stereo tone at the render rate, 32-bit float: no
    // quantization, no dither, no rate conversion. If the judge cannot verify
    // THIS, the harness is broken rather than the engine — the same role `dry`
    // plays in the sampler's matrix.
    write(
        "dry.wav".into(),
        tone_net(),
        &base(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO),
    )?;

    // ---- format x depth --------------------------------------------------
    //
    // The same tone through every container and depth the crate admits. The
    // judge decodes each and compares to `dry`: a format that silently halved
    // its samples, wrote the wrong depth, or byte-swapped shows up as a level or
    // waveform difference, none of which changes the file's length.
    //
    // FLAC + Float32 is omitted deliberately — it is a documented hard error,
    // and `tests/roundtrip.rs` asserts that it errors rather than downgrading.
    for (fmt, ext) in [
        (AudioFormat::Wav, "wav"),
        (AudioFormat::Flac(Default::default()), "flac"),
        (AudioFormat::Aiff, "aiff"),
    ] {
        for (depth, tag) in [
            (BitDepth::Int16, "i16"),
            (BitDepth::Int24, "i24"),
            (BitDepth::Float32, "f32"),
        ] {
            if matches!(fmt, AudioFormat::Flac(_)) && depth == BitDepth::Float32 {
                continue;
            }
            write(
                format!("fmt_{ext}_{tag}.{ext}"),
                tone_net(),
                &base(fmt, depth, ChannelLayout::STEREO),
            )?;
        }
    }

    // Ogg is lossy, so it gets one case and is judged spectrally, never
    // sample-wise. It ignores bit depth entirely.
    write(
        "fmt_ogg.ogg".into(),
        tone_net(),
        &base(
            AudioFormat::OggVorbis(Default::default()),
            BitDepth::Int24,
            ChannelLayout::STEREO,
        ),
    )?;

    // ---- DC, where the exact expected value is knowable -------------------
    //
    // A tone's samples depend on the oscillator's phase; a constant's do not. So
    // these are the cases the judge can hold to the quantization floor rather
    // than to a spectral tolerance, and the level is chosen so rounding and
    // truncation disagree (0.7 x 32767 = 22936.9).
    for (level, tag) in [(0.7f32, "p70"), (-0.7, "n70")] {
        for (fmt, ext) in [
            (AudioFormat::Wav, "wav"),
            (AudioFormat::Flac(Default::default()), "flac"),
            (AudioFormat::Aiff, "aiff"),
        ] {
            write(
                format!("dc_{tag}_{ext}_i16.{ext}"),
                dc_net(level),
                &base(fmt, BitDepth::Int16, ChannelLayout::STEREO),
            )?;
        }
    }

    // ---- dither ----------------------------------------------------------
    //
    // Same DC, same depth, three modes. The judge measures the noise's spread
    // and mean: the modes must differ from each other, stay inside their
    // distribution's width, and none may shift the signal.
    for (mode, tag) in [
        (Dither::Off, "off"),
        (Dither::Rectangular, "rect"),
        (Dither::Triangular, "tri"),
    ] {
        let mut cfg = base(AudioFormat::Wav, BitDepth::Int16, ChannelLayout::STEREO);
        cfg.dither = mode;
        write(format!("dither_{tag}.wav"), dc_net(0.25), &cfg)?;
    }

    // ---- resampling ------------------------------------------------------
    //
    // The real gap: the crate's own tests check that a resampled file has the
    // right *number* of frames, which a resampler writing zeros would also
    // satisfy. The judge measures the tone's frequency and level after
    // conversion, plus the noise floor around it.
    for (target, tag) in [
        (44_100.0f64, "44k1"),
        (96_000.0, "96k"),
        (22_050.0, "22k05"),
    ] {
        let mut cfg = base(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
        cfg.resample = Some(Resample::to(tutti_core::SampleRate(target)));
        write(format!("resample_{tag}.wav"), tone_net(), &cfg)?;
    }

    // The anti-alias case. 18 kHz downsampled to 22.05 k is above the new
    // Nyquist and must be attenuated, not folded down to ~4 kHz.
    {
        let mut cfg = base(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
        cfg.resample = Some(Resample::to(tutti_core::SampleRate(22_050.0)));
        write("resample_alias_22k05.wav".into(), near_nyquist_net(), &cfg)?;
    }

    // Every chunk-size preset at one ratio, so a preset that degrades the
    // conversion is visible rather than assumed equivalent.
    for (i, chunk) in tutti_export::ChunkSize::PRESETS.iter().enumerate() {
        let mut cfg = base(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
        cfg.resample = Some(Resample {
            target_rate: tutti_core::SampleRate(44_100.0),
            chunk: *chunk,
        });
        write(format!("chunk_{i}.wav"), tone_net(), &cfg)?;
    }

    // ---- channels --------------------------------------------------------
    //
    // Mono up to quad (signal in channel 0, silence elsewhere) and a 5.1 fold.
    // The judge checks the per-channel energy against the ITU matrix it computes
    // itself, rather than against the engine's own coefficients.
    write(
        "chan_mono.wav".into(),
        mono_net(),
        &base(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::MONO),
    )?;
    write(
        "chan_mono_to_quad.wav".into(),
        mono_net(),
        &base(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::QUAD),
    )?;
    write(
        "chan_stereo_to_51.wav".into(),
        tone_net(),
        &base(
            AudioFormat::Wav,
            BitDepth::Float32,
            ChannelLayout::from(6u16),
        ),
    )?;

    println!("\n{count} cases written to {}", dir.display());
    Ok(())
}
