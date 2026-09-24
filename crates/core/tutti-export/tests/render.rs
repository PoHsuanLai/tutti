//! Behavioural tests for the render + encode path.
//!
//! The crate had 45 tests and none of them rendered anything through a real
//! encoder end to end, which is how three of four output formats came to be
//! broken and an upmix export came to panic, both with a green suite. Every test
//! here fails against the code as it was.
//!
//! Gated on all four format features, not just `wav`: the cases below write
//! FLAC, Ogg and AIFF unconditionally, and an encoder whose feature is off
//! returns `UnsupportedFormat` rather than a file. `hound` — which decodes the
//! results back — is likewise only linked under `wav`.

#![cfg(all(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]

use tutti_core::{Amplitude, AudioUnit, Hz, SampleRate};
use tutti_export::{
    render_to_buffers, render_to_file, AudioFormat, BitDepth, ChannelLayout, EncodeConfig,
    ExportConfig, FrozenClock, RenderClock, RenderConfig, Resample,
};
use tutti_nodes::testing::{Const, Osc};

fn net() -> tutti_core::dsp::Net {
    let mut n = tutti_core::dsp::Net::new(0, 2);
    let id = n.push(Box::new(Const::frame(&[0.5, 0.5])));
    n.pipe_output(id);
    n
}
fn config(format: AudioFormat, bd: BitDepth, layout: ChannelLayout) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: tutti_core::SampleRate(44100.0),
            duration_seconds: 0.2,
            ..Default::default()
        },
        encode: EncodeConfig {
            format,
            bit_depth: bd,
            channels: layout,
        },
        ..Default::default()
    }
}

#[test]
fn all_four_formats_export() {
    let d = tempfile::tempdir().unwrap();
    for (f, bd, ext) in [
        (AudioFormat::Wav, BitDepth::Int24, "wav"),
        (
            AudioFormat::Flac(Default::default()),
            BitDepth::Int24,
            "flac",
        ),
        (
            AudioFormat::OggVorbis(Default::default()),
            BitDepth::Int24,
            "ogg",
        ),
        (AudioFormat::Aiff, BitDepth::Int24, "aiff"),
    ] {
        let p = d.path().join(format!("a.{ext}"));
        let r = render_to_file(
            net(),
            &config(f, bd, ChannelLayout::STEREO),
            &FrozenClock,
            &p,
        );
        match &r {
            Ok(w) => println!("{ext}: OK {} bytes", w.bytes),
            Err(e) => println!("{ext}: ERR {e}"),
        }
        assert!(r.is_ok(), "{ext} failed: {r:?}");
        assert!(
            r.unwrap().bytes > 100,
            "{ext} wrote a suspiciously small file"
        );
    }
}

#[test]
fn upmix_does_not_panic_and_leaves_extras_silent() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("q.wav");
    let mut n = tutti_core::dsp::Net::new(0, 1);
    let id = n.push(Box::new(Const::mono(0.5)));
    n.pipe_output(id);
    render_to_file(
        n,
        &config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::QUAD),
        &FrozenClock,
        &p,
    )
    .unwrap();
    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(rd.spec().channels, 4);
    let s: Vec<f32> = rd.into_samples::<f32>().map(|x| x.unwrap()).collect();
    let mut ch = [0.0f32; 4];
    for f in s.as_chunks::<4>().0 {
        for (a, &v) in ch.iter_mut().zip(f) {
            *a = a.max(v.abs());
        }
    }
    println!("mono->quad channel peaks: {ch:?}");
    assert!(ch[0] > 0.4, "ch0 carries the signal");
    for (c, &peak) in ch.iter().enumerate().skip(1) {
        assert!(peak < 1e-6, "ch{c} must be silent, got {peak}");
    }
}

/// The clock is advanced once per block, by exactly the frames produced.
///
/// This is the test the old `transport()` setter never had: sabotaging that
/// setter to discard its argument passed all 45 tests, even though its own doc
/// warned the failure mode was total silence. A clock that is never advanced
/// leaves every placed voice at beat 0.
#[test]
fn the_clock_advances_by_exactly_the_frames_rendered() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingClock(AtomicUsize);
    impl RenderClock for CountingClock {
        fn advance(&self, frames: tutti_types::Samples) {
            self.0.fetch_add(frames.get(), Ordering::Relaxed);
        }
    }

    let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
    let out = render_to_buffers(
        net(),
        &config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO),
        clock.as_ref(),
    )
    .unwrap();

    let frames = out.frames().get();
    assert_eq!(frames, 8820, "0.2 s at 44.1 kHz");
    assert_eq!(
        clock.0.load(Ordering::Relaxed),
        frames,
        "the clock must advance exactly once per rendered frame"
    );
}

/// A latency trim renders extra frames and drops them from the head, so the
/// output is still the requested length — not short by the trim.
#[test]
fn a_latency_trim_preserves_the_output_length() {
    let mut s = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
    let untrimmed = render_to_buffers(net(), &s, &FrozenClock).unwrap();

    s.render.latency = tutti_types::Samples(512);
    let trimmed = render_to_buffers(net(), &s, &FrozenClock).unwrap();

    assert_eq!(
        untrimmed.frames(),
        trimmed.frames(),
        "trimming must not shorten the output"
    );
}

/// A tail lengthens the output by exactly the tail.
///
/// The mirror of `a_latency_trim_preserves_the_output_length`, and the opposite
/// direction: a trim must not shorten the file, a tail must lengthen it. The
/// decay is audio the graph produced, so keeping it is the point — rendering it
/// and then discarding it at the sink's cap would leave this assertion at the
/// untailed length.
#[test]
fn a_tail_lengthens_the_output_by_exactly_the_tail() {
    let mut s = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
    let untailed = render_to_buffers(net(), &s, &FrozenClock).unwrap();

    let tail = tutti_types::Samples(4096);
    s.render.tail = tail;
    let tailed = render_to_buffers(net(), &s, &FrozenClock).unwrap();

    assert_eq!(
        tailed.frames().get(),
        untailed.frames().get() + tail.get(),
        "the rendered tail must reach the output, not be capped away"
    );
}

/// A convolver's tail is its impulse response's ring-out, exactly.
///
/// The expected value here comes from the DSP, not from the implementation: an
/// FIR of length `L` answers an impulse at frame 0 through frame `L - 1`, so
/// `L - 1` frames land after the input stops. This is the one node whose tail is
/// known a priori, which is what makes it worth reaching for a dev-dependency.
///
/// Read through `samples()`: every node here reports, so the graph's tail is a
/// number the caller can spend without a decision.
#[test]
fn a_convolver_reports_its_ir_ring_out() {
    let ir = vec![0.5f32; 4096];
    let mut n = tutti_core::dsp::Net::new(0, 1);
    let src = n.push(Box::new(Const::mono(0.5)));
    let conv = n.push(Box::new(tutti_nodes::ConvolverNode::with_ir(&ir)));
    n.connect(src, 0, conv, 0);
    n.pipe_output(conv);

    assert_eq!(
        tutti_export::reported_tail(&n).samples(),
        Some(tutti_types::Samples(4095)),
    );
}

/// Cascaded convolvers sum their ring-outs, because cascading convolves the two
/// responses and a ring-out is defined so that supports add without a
/// correction term.
#[test]
fn cascaded_convolvers_sum_their_tails() {
    let a = vec![0.5f32; 1024];
    let b = vec![0.5f32; 2048];
    let mut n = tutti_core::dsp::Net::new(0, 1);
    let src = n.push(Box::new(Const::mono(0.5)));
    let first = n.push(Box::new(tutti_nodes::ConvolverNode::with_ir(&a)));
    let second = n.push(Box::new(tutti_nodes::ConvolverNode::with_ir(&b)));
    n.connect(src, 0, first, 0);
    n.connect(first, 0, second, 0);
    n.pipe_output(second);

    // 1023 + 2047, which is the ring-out of the 3071-sample cascaded response.
    assert_eq!(
        tutti_export::reported_tail(&n).samples(),
        Some(tutti_types::Samples(3070)),
    );
}

/// One un-reporting node is enough to make the graph's figure a partial one.
///
/// `known()` still gives the sum over what spoke, so the two accessors disagree
/// — which is the whole reason there are two. The unreporting node has to be
/// constructed deliberately now that the stock fundsp nodes all answer.
#[test]
fn one_silent_node_makes_the_figure_partial_without_losing_it() {
    /// A node that has never been taught to report a tail: the `AudioUnit`
    /// default, which is what any newly-written node starts as.
    #[derive(Clone, Default)]
    struct Unreporting;

    impl AudioUnit for Unreporting {
        fn reset(&mut self) {}
        fn set_sample_rate(&mut self, _: tutti_core::SampleRate) {}
        fn tick(&mut self, input: &[f32], output: &mut [f32]) {
            output[0] = input[0];
        }
        fn process(
            &mut self,
            size: usize,
            input: &tutti_core::BufferRef,
            output: &mut tutti_core::BufferMut,
        ) {
            for i in 0..size {
                output.set_f32(0, i, input.at_f32(0, i));
            }
        }
        fn inputs(&self) -> usize {
            1
        }
        fn outputs(&self) -> usize {
            1
        }
        fn route(&mut self, input: &tutti_core::SignalFrame, _: f64) -> tutti_core::SignalFrame {
            input.clone()
        }
        fn get_id(&self) -> u64 {
            0xDEAD_BEEF
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
            self
        }
        fn footprint(&self) -> usize {
            std::mem::size_of::<Self>()
        }
    }

    let ir = vec![0.5f32; 4096];
    let mut n = tutti_core::dsp::Net::new(0, 1);
    let src = n.push(Box::new(Const::mono(0.5)));
    let quiet = n.push(Box::new(Unreporting));
    let conv = n.push(Box::new(tutti_nodes::ConvolverNode::with_ir(&ir)));
    n.connect(src, 0, quiet, 0);
    n.connect(quiet, 0, conv, 0);
    n.pipe_output(conv);

    let reported = tutti_export::reported_tail(&n);
    assert_eq!(reported.unknown_nodes(), 1, "one node never answered");
    assert_eq!(
        reported.samples(),
        None,
        "so the graph's tail cannot be spent as a number"
    );
    assert_eq!(
        reported.known(),
        tutti_types::Samples(4095),
        "but what the convolver reported is still available"
    );
}

/// The stock nodes now answer, so an ordinary graph reports a spendable tail.
///
/// This is what teaching `AudioNode` to report bought: before it, a single
/// `dc` source left `samples()` refusing for every real project, because one
/// unreporting node on the output path makes the whole figure unspendable.
#[test]
fn a_graph_of_stock_nodes_reports_a_spendable_tail() {
    let reported = tutti_export::reported_tail(&net());
    assert_eq!(reported.unknown_nodes(), 0);
    assert_eq!(reported.samples(), Some(tutti_types::Samples(0)));
}

/// A stereo integrator: `y[n] = y[n-1] + x[n]`, per channel.
///
/// The smallest node that genuinely never decays — a feedback loop with a gain
/// of exactly one, the limit of the FDN reverb (fundsp's `reverb_stereo`) this
/// test used to reach for. The engine ships no node that reports
/// [`Tail::Unbounded`]: `ConvolverNode`, its reverb, is an FIR and reports a
/// finite ring-out (the cases above). So the property under test — that
/// `resolve` spends exactly the caller's cap on a graph that never decays —
/// needs a node that says so, and this one is honest about it.
///
/// [`Tail::Unbounded`]: tutti_types::Tail::Unbounded
#[derive(Clone, Default)]
struct Integrator([f32; 2]);

impl AudioUnit for Integrator {
    fn inputs(&self) -> usize {
        2
    }
    fn outputs(&self) -> usize {
        2
    }
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        for c in 0..2 {
            self.0[c] += input[c];
            output[c] = self.0[c];
        }
    }
    fn process(
        &mut self,
        size: usize,
        input: &tutti_core::BufferRef,
        output: &mut tutti_core::BufferMut,
    ) {
        for i in 0..size {
            for c in 0..2 {
                self.0[c] += input.at_f32(c, i);
                output.set_f32(c, i, self.0[c]);
            }
        }
    }
    fn route(&mut self, input: &tutti_core::SignalFrame, _: f64) -> tutti_core::SignalFrame {
        input.clone()
    }
    fn get_id(&self) -> u64 {
        tutti_core::mnemonic(b"TSTINTEG")
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn tail(&mut self) -> tutti_types::Tail {
        tutti_types::Tail::Unbounded
    }
    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

/// A graph that never decays resolves to exactly the caller's bound.
///
/// A feedback loop re-enters its own output, so it has no frame count of its
/// own — the cap is the whole answer, and it is the caller's choice rather than
/// a figure the engine invented.
///
/// Mutation: `Integrator` reporting `Tail::None` fails the `is_unbounded`
/// assertion — the graph is then spendable and `resolve` ignores the cap.
#[test]
fn resolving_an_unbounded_graph_spends_the_cap() {
    let mut n = tutti_core::dsp::Net::new(0, 2);
    let src = n.push(Box::new(Const::frame(&[0.5, 0.5])));
    let rev = n.push(Box::new(Integrator::default()));
    n.connect(src, 0, rev, 0);
    n.connect(src, 1, rev, 1);
    n.pipe_output(rev);

    let reported = tutti_export::reported_tail(&n);
    let cap = tutti_types::Samples(384_000);
    assert!(
        reported.is_unbounded(),
        "a unity-gain feedback loop never decays on its own"
    );
    assert_eq!(reported.samples(), None, "so there is no count to spend");
    assert_eq!(reported.resolve(cap), cap);
}

/// A fully-reported graph resolves to its own figure, not the cap.
///
/// The cap bounds; it does not replace. Spending it on a graph that answered
/// would append silence — 4095 frames is 0.09 s, and the cap here is eight
/// seconds.
#[test]
fn resolving_a_reported_graph_keeps_its_own_figure() {
    let ir = vec![0.5f32; 4096];
    let mut n = tutti_core::dsp::Net::new(0, 1);
    let src = n.push(Box::new(Const::mono(0.5)));
    let conv = n.push(Box::new(tutti_nodes::ConvolverNode::with_ir(&ir)));
    n.connect(src, 0, conv, 0);
    n.pipe_output(conv);

    let reported = tutti_export::reported_tail(&n);
    assert_eq!(
        reported.resolve(tutti_types::Samples(384_000)),
        tutti_types::Samples(4095),
        "the convolver's figure, not the cap"
    );
}

/// A stateless node declares it has no tail, rather than staying silent about
/// it.
///
/// This is what makes the mechanism usable: one unreporting node on the output
/// path makes the whole graph's tail unspendable, so declaring `None` on the
/// nodes that genuinely have none is load-bearing, not cosmetic.
#[test]
fn a_stateless_node_reports_no_tail_rather_than_an_unknown_one() {
    use tutti_core::AudioUnit;

    let mut dist = tutti_nodes::DistortionNode::new(tutti_nodes::ShapeKind::Tanh, 1.0);
    assert_eq!(dist.tail(), tutti_types::Tail::None);
}

/// `render_to_buffers` reports the rate its samples are actually at, and gives
/// back one plane per channel rather than a stereo pair.
#[test]
fn buffers_report_their_own_shape() {
    let out = render_to_buffers(
        net(),
        &config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::QUAD),
        &FrozenClock,
    )
    .unwrap();
    assert_eq!(out.channels(), 4, "a quad request yields four planes");
    assert_eq!(out.sample_rate.get(), 44100.0);
    assert!(out.planes.iter().all(|p| p.len() == out.frames().get()));
}

/// Normalization composes: measure with tutti-analysis, apply the gain it
/// reports. The crate does not hide this, which is why it can stream.
#[test]
fn a_caller_can_compose_normalization() {
    use tutti_analysis::{measure_loudness, LoudnessConfig};
    use tutti_types::{Db, Interleaved};

    // Longer than R128's 400 ms gating block, or the meter reports nothing
    // passed the gate and there is no loudness to normalize toward.
    let mut long = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
    long.render.duration_seconds = 2.0;
    let mut out = render_to_buffers(net(), &long, &FrozenClock).unwrap();

    let cfg = LoudnessConfig::new(out.sample_rate, ChannelLayout::STEREO);
    let flat = out.interleaved();
    let before = measure_loudness(&cfg, Interleaved::new(&flat, ChannelLayout::STEREO)).unwrap();
    let gain = before.gain_to(Db(-14.0), Db(-1.0));
    out.apply_gain(gain);
    let flat = out.interleaved();
    let after = measure_loudness(&cfg, Interleaved::new(&flat, ChannelLayout::STEREO)).unwrap();

    // Assert what `gain_to` actually promises, not merely that `apply_gain` is
    // linear. The earlier form checked `after ≈ before + gain`, which is
    // algebraically true for ANY gain — `gain_to` could return `Db(0.0)` and it
    // still passed, so the measure half of measure-then-apply was unverified.
    //
    // The promise is two-sided: reach the target, UNLESS the true-peak ceiling
    // binds first. This signal is a DC-ish 0.5 constant at -29.3 LUFS with a
    // -4.99 dBTP peak, so +15.3 dB would be needed for -14 LUFS — and that
    // would put the peak at +10 dBTP. The ceiling wins, and the peak is what
    // must land exactly.
    assert!(
        after.lufs.get() <= -14.0 + 0.5,
        "must never overshoot the loudness target: {before:?} -> {after:?}"
    );
    assert!(
        (after.true_peak.get() - (-1.0)).abs() < 0.5,
        "the ceiling binds here, so the peak must land on it: {after:?}"
    );

    // And the ceiling must not be an excuse to do nothing: a gain was applied.
    assert!(
        after.lufs.get() > before.lufs.get() + 1.0,
        "the reachable part of the gain must still be applied: \
         {before:?} + {gain:?} -> {after:?}"
    );
}

/// A requested resample reaches the file: the header carries the target rate,
/// and the frame count matches the converted duration.
///
/// `render_to_file` used to accept `sample_rate` and silently ignore it on the
/// in-memory path while honouring it on the file path — two terminals with the
/// same settings and different behaviour.
#[test]
fn a_resample_request_reaches_the_file() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("r.wav");
    let mut s = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
    s.render.duration_seconds = 1.0;
    s.resample = Some(Resample::to(SampleRate(48_000.0)));

    render_to_file(net(), &s, &FrozenClock, &p).unwrap();

    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(
        rd.spec().sample_rate,
        48_000,
        "header must carry the target"
    );
    let frames = rd.into_samples::<f32>().count() / 2;
    let err = (frames as i64 - 48_000).abs();
    assert!(
        err < 512,
        "expected ~48000 frames at the new rate, got {frames}"
    );
}

/// `write_buffers` completes the measure-then-apply cycle: render, normalize,
/// write. Without it a caller who normalized would have nowhere to put the
/// result — and it shares every encoder with the streaming path rather than
/// growing a second writer per format.
#[test]
fn normalized_audio_can_be_written_to_every_format() {
    let d = tempfile::tempdir().unwrap();
    let mut s = config(AudioFormat::Wav, BitDepth::Int24, ChannelLayout::STEREO);
    s.render.duration_seconds = 1.0;

    let mut audio = render_to_buffers(net(), &s, &FrozenClock).unwrap();
    audio.apply_gain(tutti_types::Db(-6.0));

    for (format, ext) in [
        (AudioFormat::Wav, "wav"),
        (AudioFormat::Flac(Default::default()), "flac"),
        (AudioFormat::OggVorbis(Default::default()), "ogg"),
        (AudioFormat::Aiff, "aiff"),
    ] {
        s.encode.format = format;
        let p = d.path().join(format!("n.{ext}"));
        let w = tutti_export::write_buffers(&audio, &s, &p).expect("write");
        assert!(w.bytes > 100, "{ext} wrote {} bytes", w.bytes);
    }

    // The written WAV must carry the gain: 0.5 at -6 dB is ~0.25. (The WAV was
    // written in the loop above; this only re-reads it.)
    let p = d.path().join("n.wav");
    let rd = hound::WavReader::open(&p).unwrap();
    let peak = rd
        .into_samples::<i32>()
        .map(|x| (x.unwrap() as f32 / 8_388_607.0).abs())
        .fold(0.0f32, f32::max);
    assert!(
        (peak - 0.25).abs() < 0.01,
        "expected ~0.25 after -6 dB, got {peak}"
    );
}

/// A resample must reach **every** format, not just the ones that happened to
/// route through the shared pump.
///
/// FLAC and Ogg used to pull from `drive` directly while still taking their
/// header rate from `encoder_rate`, so each wrote un-resampled audio under a
/// header claiming the target: a 1 s render played back 8.8% fast. WAV and AIFF
/// were correct, which is exactly why a WAV-only test could not see it.
#[test]
fn a_resample_reaches_every_format_not_just_wav() {
    let d = tempfile::tempdir().unwrap();
    let mut s = config(AudioFormat::Wav, BitDepth::Int24, ChannelLayout::STEREO);
    s.render.duration_seconds = 1.0; // 44100 frames in, 48000 expected out
    s.resample = Some(Resample::to(tutti_core::SampleRate(48_000.0)));

    // WAV is the control: it was already correct.
    let p = d.path().join("r.wav");
    render_to_file(net(), &s, &FrozenClock, &p).unwrap();
    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(rd.spec().sample_rate, 48_000);
    let wav_frames = rd.into_samples::<i32>().count() / 2;
    assert!(
        (wav_frames as i64 - 48_000).abs() < 512,
        "wav: expected ~48000 frames, got {wav_frames}"
    );

    // FLAC: the header claims 48k, so the payload must actually BE 48k. If the
    // resample were skipped the file would hold 44100 frames.
    s.encode.format = AudioFormat::Flac(Default::default());
    let pf = d.path().join("r.flac");
    render_to_file(net(), &s, &FrozenClock, &pf).unwrap();
    let flac_frames = flac_frame_count(&pf);
    assert!(
        (flac_frames as i64 - 48_000).abs() < 512,
        "flac: header says 48000 but payload holds {flac_frames} frames \
         (44100 means the resample was skipped)"
    );

    // Ogg: same property, read from the final page's granule position.
    s.encode.format = AudioFormat::OggVorbis(Default::default());
    let po = d.path().join("r.ogg");
    render_to_file(net(), &s, &FrozenClock, &po).unwrap();
    let ogg_frames = ogg_granule(&po);
    assert!(
        (ogg_frames as i64 - 48_000).abs() < 2048,
        "ogg: header says 48000 but granule reports {ogg_frames} frames"
    );
}

/// `write_buffers` must resample from the rate the FRAMES are at, not from
/// whatever `config.render.sample_rate` happens to hold.
///
/// The frames handed to `write_buffers` already exist; `config.render` describes
/// a render that already happened, and may be a different rate entirely. Reading
/// it made the output 2.18x too long and pitched down, under a header that said
/// otherwise.
#[test]
fn write_buffers_resamples_from_the_frames_own_rate() {
    let d = tempfile::tempdir().unwrap();

    // Render at 96k...
    let mut render_cfg = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
    render_cfg.render.sample_rate = tutti_core::SampleRate(96_000.0);
    render_cfg.render.duration_seconds = 1.0;
    let audio = render_to_buffers(net(), &render_cfg, &FrozenClock).unwrap();
    assert_eq!(audio.sample_rate.get(), 96_000.0);

    // ...then write with a config whose `render` half is left at its DEFAULT
    // 44100 — the case the doc tells a caller is fine to ignore.
    let mut write_cfg = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
    write_cfg.resample = Some(Resample::to(tutti_core::SampleRate(48_000.0)));
    assert_eq!(
        write_cfg.render.sample_rate.get(),
        44_100.0,
        "this test is only meaningful while the config's render rate differs"
    );

    let p = d.path().join("w.wav");
    tutti_export::write_buffers(&audio, &write_cfg, &p).unwrap();

    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(rd.spec().sample_rate, 48_000);
    let frames = rd.into_samples::<f32>().count() / 2;
    // 96k -> 48k halves the length. Reading 44100 as the source instead would
    // give ~104_490 frames.
    assert!(
        (frames as i64 - 48_000).abs() < 512,
        "expected ~48000 frames (96k halved), got {frames} \
         (~104490 means it resampled from the config's rate, not the audio's)"
    );
}

/// Peak sample of a written float WAV.
fn file_peak(path: &std::path::Path) -> f32 {
    hound::WavReader::open(path)
        .unwrap()
        .into_samples::<f32>()
        .map(|s| s.unwrap().abs())
        .fold(0.0f32, f32::max)
}

/// `render_normalized_to_file` reaches the requested loudness — the whole point
/// of the two-pass path. The un-normalized render of the same graph is the
/// control: equal peaks would mean the normalization did nothing.
#[test]
fn normalized_export_lifts_the_level_toward_the_target() {
    use tutti_export::{render_normalized_to_file, Normalize};
    use tutti_types::Db;

    let d = tempfile::tempdir().unwrap();
    // Longer than R128's 400 ms gating block, or nothing passes the gate.
    let mut cfg = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
    cfg.render.duration_seconds = 2.0;

    let plain = d.path().join("plain.wav");
    render_to_file(net(), &cfg, &FrozenClock, &plain).unwrap();

    let normalized = d.path().join("normalized.wav");
    render_normalized_to_file(
        net(),
        &cfg,
        &FrozenClock,
        Normalize::lufs(Db(-14.0)),
        &normalized,
    )
    .unwrap();

    let (before, after) = (file_peak(&plain), file_peak(&normalized));
    assert!(
        after > before,
        "normalization must lift this signal: {before} -> {after}"
    );
    assert!(after <= 1.0, "and must never clip past full scale: {after}");
}

/// `Normalize::Peak` targets TRUE peak (4x oversampled), which is why it routes
/// through the R128 meter rather than a `fold(max, abs)` over the planes.
///
/// So the file must be re-**measured**, not merely re-read: for this signal the
/// two readings differ by ~1 dB. A 0.5 constant has a −6.02 dB *sample* peak but
/// a −4.99 dBTP *true* peak — the oversampling filter rings at the buffer's hard
/// leading edge. Asserting the sample peak against a dBTP target would be
/// comparing two different quantities, and would have to be loosened by exactly
/// the amount the feature is supposed to catch.
#[test]
fn peak_normalization_lands_on_the_requested_dbtp() {
    use tutti_analysis::{measure_loudness, LoudnessConfig};
    use tutti_export::{render_normalized_to_file, Normalize};
    use tutti_types::{Db, Interleaved};

    let d = tempfile::tempdir().unwrap();
    let mut cfg = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
    cfg.render.duration_seconds = 2.0;

    let path = d.path().join("peak.wav");
    render_normalized_to_file(net(), &cfg, &FrozenClock, Normalize::peak(Db(-1.0)), &path).unwrap();

    let samples: Vec<f32> = hound::WavReader::open(&path)
        .unwrap()
        .into_samples::<f32>()
        .map(|s| s.unwrap())
        .collect();
    let meter = LoudnessConfig::new(cfg.render.sample_rate, ChannelLayout::STEREO);
    let measured =
        measure_loudness(&meter, Interleaved::new(&samples, ChannelLayout::STEREO)).unwrap();

    assert!(
        (measured.true_peak.get() - (-1.0)).abs() < 0.2,
        "the written file's TRUE peak must land on the -1 dBTP target, got {:?}",
        measured.true_peak
    );
    // And the sample peak sits BELOW it — the gap is the oversampling headroom
    // a sample-peak normalizer would have handed to the clipper instead.
    assert!(
        file_peak(&path) < 0.891,
        "a true-peak target must leave sample-peak headroom, got {}",
        file_peak(&path)
    );
}

/// The trap `loudness.rs::a_reading_is_always_finite_even_below_the_gate` names,
/// reached through the export path: a render SHORTER than R128's 400 ms gating
/// block reports `-inf` LUFS, and a gain derived from it would be `NaN` — which
/// multiplies the whole file into `NaN`. The meter clamps to the gate, so this
/// must produce a finite, readable file.
#[test]
fn a_sub_gating_block_render_normalizes_without_poisoning_the_signal() {
    use tutti_export::{render_normalized_to_file, Normalize};
    use tutti_types::Db;

    let d = tempfile::tempdir().unwrap();
    // 0.2 s — half the 400 ms gating block.
    let cfg = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
    assert!(
        cfg.render.duration_seconds < 0.4,
        "this test is only meaningful under the gating block"
    );

    let path = d.path().join("short.wav");
    render_normalized_to_file(net(), &cfg, &FrozenClock, Normalize::lufs(Db(-14.0)), &path)
        .unwrap();

    let samples: Vec<f32> = hound::WavReader::open(&path)
        .unwrap()
        .into_samples::<f32>()
        .map(|s| s.unwrap())
        .collect();
    assert!(!samples.is_empty(), "a short render still writes frames");
    assert!(
        samples.iter().all(|s| s.is_finite()),
        "a sub-gate reading must not poison the signal into NaN"
    );
}

/// R128 meters any channel count, so a surround export normalizes against its
/// own loudness. Pins the removal of the old stereo-only restriction: it must
/// not error, and must not silently switch to a different metric.
#[test]
fn surround_normalizes_rather_than_falling_back_to_peak() {
    use tutti_export::{render_normalized_to_file, Normalize};
    use tutti_types::Db;

    let d = tempfile::tempdir().unwrap();
    let mut n = tutti_core::dsp::Net::new(0, 6);
    let id = n.push(Box::new(Const::frame(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6])));
    n.pipe_output(id);

    let mut cfg = config(
        AudioFormat::Wav,
        BitDepth::Float32,
        ChannelLayout::from(6u16),
    );
    cfg.render.duration_seconds = 2.0;

    let path = d.path().join("surround.wav");
    render_normalized_to_file(n, &cfg, &FrozenClock, Normalize::lufs(Db(-14.0)), &path)
        .expect("5.1 normalization must not be rejected");

    let reader = hound::WavReader::open(&path).unwrap();
    assert_eq!(reader.spec().channels, 6, "all six channels must survive");
}

/// Frame count from a FLAC STREAMINFO block (bytes 21..27, 36 bits).
fn flac_frame_count(path: &std::path::Path) -> u64 {
    let b = std::fs::read(path).unwrap();
    assert_eq!(&b[0..4], b"fLaC", "not a FLAC file");
    // 4 magic + 4 block header, then STREAMINFO; total-samples is 36 bits
    // starting 13 bytes into it.
    let si = &b[8..];
    ((si[13] as u64 & 0x0F) << 32)
        | (si[14] as u64) << 24
        | (si[15] as u64) << 16
        | (si[16] as u64) << 8
        | (si[17] as u64)
}

/// Granule position of the last Ogg page — the total frames encoded.
///
/// Walks pages by seeking the `OggS` capture pattern rather than stepping
/// blindly: a page body can contain those four bytes, and a naive walk that
/// mis-steps once reads a random eight bytes as a granule (it reported 3830784
/// for a 96000-frame file while I was writing this).
fn ogg_granule(path: &std::path::Path) -> u64 {
    let b = std::fs::read(path).unwrap();
    let mut last = 0i64;
    let mut i = 0usize;
    while let Some(off) = find_from(&b, b"OggS", i) {
        if off + 27 > b.len() {
            break;
        }
        let gran = i64::from_le_bytes(b[off + 6..off + 14].try_into().unwrap());
        let segs = b[off + 26] as usize;
        if off + 27 + segs > b.len() {
            break;
        }
        // -1 marks a page that completes no packet; it is not a frame count.
        if gran >= 0 {
            last = gran;
        }
        let body: usize = b[off + 27..off + 27 + segs]
            .iter()
            .map(|&s| s as usize)
            .sum();
        i = off + 27 + segs + body;
    }
    last as u64
}

fn find_from(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Peak of an integer WAV, as a fraction of full scale.
fn int_file_peak(path: &std::path::Path) -> f32 {
    let reader = hound::WavReader::open(path).unwrap();
    let bits = reader.spec().bits_per_sample;
    let full = (1i64 << (bits - 1)) as f32;
    reader
        .into_samples::<i32>()
        .map(|s| (s.unwrap() as f32).abs() / full)
        .fold(0.0f32, f32::max)
}

/// **Normalizing silence must write silence.**
///
/// `render_to_buffers` used to dither, so at an integer depth the planes handed
/// to the meter were not silent — they carried ±1 LSB of noise. The meter read
/// that as the signal (~-86 dBTP), the gain came back at ~+86 dB, and
/// `apply_gain` amplified the noise: a "normalized" export of silence landed
/// near full scale.
///
/// Dither belongs at the encode boundary, where the LSB is known; buffers are
/// `f32` and quantize to nothing.
#[test]
fn normalizing_silence_at_an_integer_depth_does_not_amplify_dither() {
    use tutti_export::{render_normalized_to_file, Normalize};
    use tutti_types::Db;

    let silence = || {
        let mut n = tutti_core::dsp::Net::new(0, 2);
        let id = n.push(Box::new(Const::frame(&[0.0, 0.0])));
        n.pipe_output(id);
        n
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("silence.wav");

    let mut cfg = config(AudioFormat::Wav, BitDepth::Int16, ChannelLayout::STEREO);
    cfg.dither = tutti_export::Dither::Triangular;

    render_normalized_to_file(
        silence(),
        &cfg,
        &FrozenClock,
        Normalize::peak(Db(-1.0)),
        &path,
    )
    .unwrap();

    let peak = int_file_peak(&path);
    assert!(
        peak <= 2.0 / 32768.0,
        "normalized silence must stay at the dither floor, got {peak} of full scale"
    );
}

/// **The dBTP target must hold in the written file, resample included.**
///
/// Sample-rate conversion moves the true peak — its interpolation overshoots
/// between the original samples — so a gain measured at the render rate and
/// applied to audio written at another lands off target.
///
/// The shift is small (~0.16 dB on this signal, measured) but systematic and
/// always upward, which is what makes it dangerous: it eats the safety margin a
/// ceiling exists to provide, and at a 0 dBTP target it clips outright. The
/// tolerance below is deliberately TIGHTER than the shift — a looser one passes
/// whether or not the conversion happens before the measurement, which is
/// exactly how this bug survived its first test.
#[test]
fn a_resampled_normalized_export_still_lands_on_its_dbtp_target() {
    use tutti_analysis::{measure_loudness, LoudnessConfig};
    use tutti_export::{render_normalized_to_file, Normalize};
    use tutti_types::{Db, Interleaved};

    // Energy near Nyquist, with its true peak *between* samples, is what SRC
    // overshoots on; a DC constant barely moves and would hide the bug entirely.
    //
    // A sine at a quarter of the 44.1 kHz render rate, started at 0.546 turns so
    // the samples straddle the peak unevenly (-0.28, -0.93, +0.28, +0.93 of it).
    // That is exactly what the fundsp `square_hz(11025.0)` this test used to
    // build rendered: a band-limited square at fs/4 has no harmonic below
    // Nyquist but its fundamental, and its phase came from the graph's hash —
    // measured, not assumed. The phase is load-bearing: at 1/8 turn the samples
    // sit symmetrically about the peak, the render-rate true-peak
    // estimate is already right, and the test passes with the measure-before-
    // resample bug put back. The amplitude is not: peak normalization divides
    // it out.
    let square = || {
        let mut n = tutti_core::dsp::Net::new(0, 2);
        let tone = Osc::sine(Hz(11025.0))
            .with_phase(tutti_core::Phase(0.546))
            .with_amplitude(Amplitude(0.98))
            .with_layout(ChannelLayout::STEREO);
        let id = n.push(Box::new(tone));
        n.pipe_output(id);
        n
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("resampled.wav");

    let mut cfg = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
    cfg.resample = Some(Resample::to(tutti_core::SampleRate(48_000.0)));

    render_normalized_to_file(
        square(),
        &cfg,
        &FrozenClock,
        Normalize::peak(Db(-1.0)),
        &path,
    )
    .unwrap();

    // Re-measure the FILE, at the rate it was actually written at.
    let reader = hound::WavReader::open(&path).unwrap();
    let rate = reader.spec().sample_rate;
    assert_eq!(rate, 48_000, "the resample must have reached the file");
    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();

    let meter = LoudnessConfig::new(tutti_core::SampleRate(rate as f64), ChannelLayout::STEREO);
    let measured = measure_loudness(&meter, Interleaved::new(&samples, ChannelLayout::STEREO))
        .expect("stereo at 48k is measurable");

    assert!(
        (measured.true_peak.get() - (-1.0)).abs() < 0.05,
        "written file must sit at the -1 dBTP target, measured {:?} — a gain \
         chosen before the resample lands ~0.16 dB high",
        measured.true_peak
    );
}

/// A signal the meter cannot read is an error, not a silently un-normalized
/// file. `Written` carries no field saying the gain was skipped, so reporting
/// success would lose the fact entirely.
#[test]
fn an_unmeasurable_rate_fails_rather_than_writing_un_normalized_audio() {
    use tutti_export::{render_normalized_to_file, Normalize};
    use tutti_types::Db;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unmeasurable.wav");

    // R128 accepts 16 Hz - 2.8 MHz; 4 MHz is outside it.
    let mut cfg = config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::STEREO);
    cfg.render.sample_rate = tutti_core::SampleRate(4_000_000.0);
    cfg.render.duration_seconds = 0.0005;

    let err =
        render_normalized_to_file(net(), &cfg, &FrozenClock, Normalize::peak(Db(-1.0)), &path)
            .expect_err("an unmeasurable rate must not report success");

    assert!(
        matches!(err, tutti_export::Error::Unmeasurable(_)),
        "expected Unmeasurable, got {err:?}"
    );
}

/// **The width bug.** A master whose channel count is not one of 1/2/4/6/8/12 —
/// `Multi(3)`, `Multi(5)`, `Multi(7)` — must export.
///
/// It could not, and this test could not have been written before. The render
/// pipeline was const-generic in its frame width, and `dispatch_channels!`
/// resolved the runtime `ChannelLayout` to one of exactly six monomorphizations,
/// returning `Error::UnsupportedChannels` for anything else. The app passes
/// `ChannelLayout::from(master_width)` straight through, so a 3- or 5-wide
/// master failed at the entry point with no way for a caller to work around it.
///
/// Asserted through the file header and the samples, not the return value: "no
/// error" would also pass if the export quietly wrote a stereo file.
#[test]
fn a_width_the_old_dispatch_rejected_now_exports() {
    let d = tempfile::tempdir().unwrap();

    for width in [3u16, 5, 7, 9] {
        let layout = ChannelLayout::from(width);
        assert!(
            !matches!(
                layout,
                ChannelLayout::MONO | ChannelLayout::STEREO | ChannelLayout::QUAD
            ),
            "width {width} should be an unnamed layout"
        );

        // A net as wide as the file, carrying a distinct constant per channel so
        // a dropped or duplicated channel is visible.
        let mut n = tutti_core::dsp::Net::new(0, width as usize);
        for c in 0..width as usize {
            let id = n.push(Box::new(Const::mono(0.1 + 0.05 * c as f32)));
            n.connect_output(id, 0, c);
        }

        let p = d.path().join(format!("w{width}.wav"));
        render_to_file(
            n,
            &config(AudioFormat::Wav, BitDepth::Float32, layout),
            &FrozenClock,
            &p,
        )
        .unwrap_or_else(|e| panic!("width {width} failed to export: {e}"));

        let reader = hound::WavReader::open(&p).unwrap();
        assert_eq!(
            reader.spec().channels,
            width,
            "the file must carry {width} channels"
        );

        let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
        assert!(!samples.is_empty(), "width {width} wrote no audio");
        assert_eq!(
            samples.len() % width as usize,
            0,
            "width {width}: not a whole number of frames"
        );

        // Every channel carries its OWN constant — proof the frames are
        // interleaved at the right stride, which is the failure mode a
        // frame-index-vs-sample-index slip at an odd width would produce.
        let mid = samples.len() / width as usize / 2;
        for c in 0..width as usize {
            let got = samples[mid * width as usize + c];
            let want = 0.1 + 0.05 * c as f32;
            assert!(
                (got - want).abs() < 1e-3,
                "width {width} channel {c}: got {got}, expected {want}"
            );
        }
    }
}

/// The same widths through the in-memory path and back out, so `write_buffers`
/// and `render_to_buffers` are covered too — all three entry points went through
/// the same rejecting dispatch.
#[test]
fn an_odd_width_round_trips_through_buffers() {
    use tutti_export::write_buffers;

    let d = tempfile::tempdir().unwrap();
    for width in [3u16, 5] {
        let layout = ChannelLayout::from(width);
        let mut n = tutti_core::dsp::Net::new(0, width as usize);
        for c in 0..width as usize {
            let id = n.push(Box::new(Const::mono(0.25)));
            n.connect_output(id, 0, c);
        }

        let cfg = config(AudioFormat::Wav, BitDepth::Float32, layout);
        let rendered = render_to_buffers(n, &cfg, &FrozenClock)
            .unwrap_or_else(|e| panic!("width {width}: render_to_buffers failed: {e}"));
        assert_eq!(rendered.channels(), width as usize);
        assert_eq!(rendered.layout(), layout);

        let p = d.path().join(format!("buf{width}.wav"));
        write_buffers(&rendered, &cfg, &p)
            .unwrap_or_else(|e| panic!("width {width}: write_buffers failed: {e}"));
        assert_eq!(hound::WavReader::open(&p).unwrap().spec().channels, width);
    }
}

/// An odd width must survive a resample too — that path deinterleaves into
/// planes and re-interleaves them, so it is where a stride mistake at a width
/// the old code never saw would surface.
#[test]
fn an_odd_width_survives_a_resample() {
    let d = tempfile::tempdir().unwrap();
    let width = 5u16;
    let mut n = tutti_core::dsp::Net::new(0, width as usize);
    for c in 0..width as usize {
        let id = n.push(Box::new(Const::mono(0.1 + 0.05 * c as f32)));
        n.connect_output(id, 0, c);
    }

    let mut cfg = config(
        AudioFormat::Wav,
        BitDepth::Float32,
        ChannelLayout::from(width),
    );
    cfg.resample = Some(Resample::to(48_000.0));

    let p = d.path().join("resampled5.wav");
    render_to_file(n, &cfg, &FrozenClock, &p).expect("a 5-wide resampled export");

    let reader = hound::WavReader::open(&p).unwrap();
    assert_eq!(reader.spec().channels, width);
    assert_eq!(reader.spec().sample_rate, 48_000);

    let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
    let frames = samples.len() / width as usize;
    assert!(frames > 0, "resampled export wrote no audio");
    // Mid-file, past the resampler's transient: each channel still carries its
    // own constant, so the re-interleave used the right stride.
    let mid = frames / 2;
    for c in 0..width as usize {
        let got = samples[mid * width as usize + c];
        let want = 0.1 + 0.05 * c as f32;
        assert!(
            (got - want).abs() < 1e-2,
            "resampled channel {c}: got {got}, expected {want}"
        );
    }
}

/// Zero channels is the one width a render still refuses — the re-purposed
/// `UnsupportedChannels`. `Multi(0)` is a representable `ChannelLayout` (the
/// empty bus), so it has to be rejected somewhere rather than dividing by zero
/// in the frame stride.
#[test]
fn a_zero_width_export_is_rejected_not_a_divide_by_zero() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("zero.wav");
    let err = render_to_file(
        net(),
        &config(AudioFormat::Wav, BitDepth::Float32, ChannelLayout::EMPTY),
        &FrozenClock,
        &p,
    )
    .expect_err("a zero-channel render must not report success");
    assert!(
        matches!(err, tutti_export::Error::UnsupportedChannels(0)),
        "expected UnsupportedChannels(0), got {err:?}"
    );
}
