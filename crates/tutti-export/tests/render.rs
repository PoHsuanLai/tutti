//! Behavioural tests for the render + encode path.
//!
//! The crate had 45 tests and none of them rendered anything through a real
//! encoder end to end, which is how three of four output formats came to be
//! broken and an upmix export came to panic, both with a green suite. Every test
//! here fails against the code as it was.
//!
//! Gated on all four format features, not just `wav`: the cases below write
//! FLAC, Ogg and AIFF unconditionally, and a `Format` variant exists only
//! with its feature. `hound` decodes the results back.
//!
//! Every graph here is built with `tutti_graph::GraphBuilder` and rendered
//! as a `RenderGraph`; the golden-digest pins live in `graph_source.rs`.

#![cfg(all(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]

mod support;
use crate::support::{EncodeConfig, ExportConfig, RenderConfig};

use crate::support::{render_to_buffers, render_to_file};
use tutti_codec::{FlacLevel, Format, IntDepth, Pcm, Quality};
use tutti_export::{RenderGraph, Resample, Setup};
use tutti_graph::{Cx, ForkByClone, GraphBuilder, Io, Node, Prepare, Shape, Status};
use tutti_nodes::testing::{Const, Osc};
use tutti_transport::{FrozenClock, RenderClock};
use tutti_types::ChannelLayout;
use tutti_types::Seconds;
use tutti_types::{Amplitude, ChannelTopology, Drive, Hz, SampleRate};

/// The rate [`config`] renders at.
const RATE: SampleRate = SampleRate(44_100.0);

/// `g`, built for an export at `rate` — which must be the config's render
/// rate: a graph prepared at another is refused, not re-rated.
fn built(g: GraphBuilder, rate: SampleRate) -> RenderGraph {
    built_with(g, rate, Setup::new())
}

/// [`built`] with the root declared (or not) as `setup` says.
fn built_with(g: GraphBuilder, rate: SampleRate, setup: Setup) -> RenderGraph {
    let (editor, executor) = g.build(RenderGraph::prepare(rate)).expect("builds");
    RenderGraph::assemble(editor, executor, setup).expect("built together")
}

/// A stereo DC at 0.5, built for an export at `rate`.
fn dc(rate: SampleRate) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Const::frame(&[0.5, 0.5]));
    g.pipe_output(id);
    built(g, rate)
}

/// FLAC at 24 bit and the default level.
fn flac() -> Format {
    Format::Flac {
        depth: IntDepth::Int24,
        level: FlacLevel::default(),
    }
}

/// Ogg Vorbis at the default quality.
fn vorbis() -> Format {
    Format::Vorbis {
        quality: Quality::default(),
    }
}

fn config(format: Format, layout: ChannelLayout) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: RATE,
            duration_seconds: Seconds(0.2),
            ..Default::default()
        },
        encode: EncodeConfig {
            format,
            channels: layout,
            topology: None,
        },
        ..Default::default()
    }
}

#[test]
fn all_four_formats_export() {
    let d = tempfile::tempdir().unwrap();
    for (f, ext) in [
        (Format::Wav(Pcm::Int24), "wav"),
        (flac(), "flac"),
        (vorbis(), "ogg"),
        (Format::Aiff(Pcm::Int24), "aiff"),
    ] {
        let p = d.path().join(format!("a.{ext}"));
        let r = render_to_file(
            dc(RATE),
            &config(f, ChannelLayout::STEREO),
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

/// A mono graph exported to quad is the front pair, and the rears stay silent.
#[test]
fn upmix_does_not_panic_and_leaves_extras_silent() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("q.wav");
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let id = g.add(Const::mono(0.5));
    g.pipe_output(id);
    render_to_file(
        built(g, RATE),
        &config(Format::Wav(Pcm::Float32), ChannelLayout::QUAD),
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
    assert!(
        ch[0] > 0.4 && ch[1] > 0.4,
        "the front pair carries the signal"
    );
    for (c, &peak) in ch.iter().enumerate().skip(2) {
        assert!(peak < 1e-6, "ch{c} must be silent, got {peak}");
    }
}

/// The clock is advanced once per block, by exactly the frames produced.
///
/// A clock that is never advanced leaves every placed voice at beat 0, and
/// the failure mode is total silence.
#[test]
fn the_clock_advances_by_exactly_the_frames_rendered() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingClock(AtomicUsize);
    impl RenderClock for CountingClock {
        fn advance(&self, frames: tutti_types::Samples) {
            self.0.fetch_add(frames.get(), Ordering::Relaxed);
        }
        // A stopped transport at beat 0: nothing in this graph reads it.
        // That the graph is handed the clock's transport block by block is
        // `graph_source.rs`'s `the_graph_reads_the_render_clocks_transport`.
        fn graph_block(&self) -> (tutti_graph::Transport, tutti_graph::TransportChanges) {
            Default::default()
        }
    }

    let clock = Arc::new(CountingClock(AtomicUsize::new(0)));
    let out = render_to_buffers(
        dc(RATE),
        &config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO),
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
    let mut s = config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO);
    let untrimmed = render_to_buffers(dc(RATE), &s, &FrozenClock).unwrap();

    s.render.latency = tutti_types::Samples(512);
    let trimmed = render_to_buffers(dc(RATE), &s, &FrozenClock).unwrap();

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
    let mut s = config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO);
    let untailed = render_to_buffers(dc(RATE), &s, &FrozenClock).unwrap();

    let tail = tutti_types::Samples(4096);
    s.render.tail = tail;
    let tailed = render_to_buffers(dc(RATE), &s, &FrozenClock).unwrap();

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
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let src = g.add(Const::mono(0.5));
    let conv = g
        .add_with_controls(tutti_nodes::ConvolverNode::with_ir(&ir))
        .0;
    g.connect(src, 0, conv, 0).pipe_output(conv);

    assert_eq!(
        built(g, RATE).reported_tail().samples(),
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
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let src = g.add(Const::mono(0.5));
    let first = g
        .add_with_controls(tutti_nodes::ConvolverNode::with_ir(&a))
        .0;
    let second = g
        .add_with_controls(tutti_nodes::ConvolverNode::with_ir(&b))
        .0;
    g.connect(src, 0, first, 0)
        .connect(first, 0, second, 0)
        .pipe_output(second);

    // 1023 + 2047, which is the ring-out of the 3071-sample cascaded response.
    assert_eq!(
        built(g, RATE).reported_tail().samples(),
        Some(tutti_types::Samples(3070)),
    );
}

/// One un-reporting node is enough to make the graph's figure a partial one.
///
/// `known()` still gives the sum over what spoke, so the two accessors disagree
/// — which is the whole reason there are two. The unreporting node has to be
/// constructed deliberately now that the engine's stock nodes all answer.
#[test]
fn one_silent_node_makes_the_figure_partial_without_losing_it() {
    /// A node that does not know its tail (`Tail::Unknown`): what a node that
    /// has not measured its own ring-out declares.
    #[derive(Clone, Default)]
    struct Unreporting;

    impl Node for Unreporting {
        fn shape(&self) -> Shape {
            Shape::through_unassigned(ChannelLayout::MONO).with_tail(tutti_types::Tail::Unknown)
        }
        fn prepare(&mut self, _: &Prepare) {}
        fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
            let (ins, mut outs) = io.split();
            outs.get(0).copy_from_slice(ins.get(0));
            Status::Modified
        }
        fn reset(&mut self) {}
    }

    let ir = vec![0.5f32; 4096];
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let src = g.add(Const::mono(0.5));
    let quiet = g.add(ForkByClone(Unreporting));
    let conv = g
        .add_with_controls(tutti_nodes::ConvolverNode::with_ir(&ir))
        .0;
    g.connect(src, 0, quiet, 0)
        .connect(quiet, 0, conv, 0)
        .pipe_output(conv);

    let reported = built(g, RATE).reported_tail();
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
    let reported = dc(RATE).reported_tail();
    assert_eq!(reported.unknown_nodes(), 0);
    assert_eq!(reported.samples(), Some(tutti_types::Samples(0)));
}

/// A stereo integrator: `y[n] = y[n-1] + x[n]`, per channel.
///
/// The smallest node that genuinely never decays — a feedback loop with a gain
/// of exactly one, the limit of an FDN reverb. The engine ships no node that
/// reports
/// [`Tail::Unbounded`]: `ConvolverNode`, its reverb, is an FIR and reports a
/// finite ring-out (the cases above). So the property under test — that
/// `resolve` spends exactly the caller's cap on a graph that never decays —
/// needs a node that says so, and this one is honest about it.
///
/// [`Tail::Unbounded`]: tutti_types::Tail::Unbounded
#[derive(Clone, Default)]
struct Integrator([f32; 2]);

impl Node for Integrator {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::STEREO).with_tail(tutti_types::Tail::Unbounded)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for c in 0..2 {
            for (y, &x) in outs.get(c).iter_mut().zip(ins.get(c)) {
                self.0[c] += x;
                *y = self.0[c];
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.0 = [0.0; 2];
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
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let src = g.add(Const::frame(&[0.5, 0.5]));
    let rev = g.add(ForkByClone(Integrator::default()));
    g.connect(src, 0, rev, 0)
        .connect(src, 1, rev, 1)
        .pipe_output(rev);

    let reported = built(g, RATE).reported_tail();
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
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let src = g.add(Const::mono(0.5));
    let conv = g
        .add_with_controls(tutti_nodes::ConvolverNode::with_ir(&ir))
        .0;
    g.connect(src, 0, conv, 0).pipe_output(conv);

    let reported = built(g, RATE).reported_tail();
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
    use tutti_graph::Node;

    let dist = tutti_nodes::DistortionNode::new(tutti_nodes::ShapeKind::Tanh, Drive::new(1.0));
    assert_eq!(dist.shape().tail, tutti_types::Tail::None);
}

/// `render_to_buffers` reports the rate its samples are actually at, and gives
/// back one plane per channel rather than a stereo pair.
#[test]
fn buffers_report_their_own_shape() {
    let out = render_to_buffers(
        dc(RATE),
        &config(Format::Wav(Pcm::Float32), ChannelLayout::QUAD),
        &FrozenClock,
    )
    .unwrap();
    assert_eq!(out.channels(), 4, "a quad request yields four planes");
    assert_eq!(out.sample_rate().get(), 44100.0);
    assert!(out.planes().iter().all(|p| p.len() == out.frames().get()));
}

/// Normalization composes: measure with tutti-measure, apply the gain it
/// reports. The crate does not hide this, which is why it can stream.
#[test]
fn a_caller_can_compose_normalization() {
    use tutti_measure::{LoudnessConfig, measure_loudness};
    use tutti_types::{Db, Interleaved};

    // Longer than R128's 400 ms gating block, or the meter reports nothing
    // passed the gate and there is no loudness to normalize toward.
    let mut long = config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO);
    long.render.duration_seconds = Seconds(2.0);
    let mut out = render_to_buffers(dc(RATE), &long, &FrozenClock).unwrap();

    let cfg = LoudnessConfig::new(out.sample_rate(), ChannelLayout::STEREO);
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
/// The file path must honour the resample the config asks for.
#[test]
fn a_resample_request_reaches_the_file() {
    let d = tempfile::tempdir().unwrap();
    let p = d.path().join("r.wav");
    let mut s = config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO);
    s.render.duration_seconds = Seconds(1.0);
    s.resample = Some(Resample::to(SampleRate(48_000.0)));

    render_to_file(dc(RATE), &s, &FrozenClock, &p).unwrap();

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
    let mut s = config(Format::Wav(Pcm::Int24), ChannelLayout::STEREO);
    s.render.duration_seconds = Seconds(1.0);

    let mut audio = render_to_buffers(dc(RATE), &s, &FrozenClock).unwrap();
    audio.apply_gain(tutti_types::Db(-6.0));

    for (format, ext) in [
        (Format::Wav(Pcm::Int24), "wav"),
        (flac(), "flac"),
        (vorbis(), "ogg"),
        (Format::Aiff(Pcm::Int24), "aiff"),
    ] {
        s.encode.format = format;
        let p = d.path().join(format!("n.{ext}"));
        let w = support::write_buffers(&audio, &s, &p).expect("write");
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

/// A resample must reach every format, not just the ones that happened to
/// route through the shared pump.
///
/// An encoder that pulled from `drive` directly while taking its header rate
/// from `encoder_rate` would write un-resampled audio under a header claiming
/// the target (a 1 s render playing back 8.8% fast), which a WAV-only test
/// cannot see.
#[test]
fn a_resample_reaches_every_format_not_just_wav() {
    let d = tempfile::tempdir().unwrap();
    let mut s = config(Format::Wav(Pcm::Int24), ChannelLayout::STEREO);
    s.render.duration_seconds = Seconds(1.0); // 44100 frames in, 48000 expected out
    s.resample = Some(Resample::to(tutti_types::SampleRate(48_000.0)));

    // WAV is the control: it was already correct.
    let p = d.path().join("r.wav");
    render_to_file(dc(RATE), &s, &FrozenClock, &p).unwrap();
    let rd = hound::WavReader::open(&p).unwrap();
    assert_eq!(rd.spec().sample_rate, 48_000);
    let wav_frames = rd.into_samples::<i32>().count() / 2;
    assert!(
        (wav_frames as i64 - 48_000).abs() < 512,
        "wav: expected ~48000 frames, got {wav_frames}"
    );

    // FLAC: the header claims 48k, so the payload must actually BE 48k. If the
    // resample were skipped the file would hold 44100 frames.
    s.encode.format = flac();
    let pf = d.path().join("r.flac");
    render_to_file(dc(RATE), &s, &FrozenClock, &pf).unwrap();
    let flac_frames = flac_frame_count(&pf);
    assert!(
        (flac_frames as i64 - 48_000).abs() < 512,
        "flac: header says 48000 but payload holds {flac_frames} frames \
         (44100 means the resample was skipped)"
    );

    // Ogg: same property, read from the final page's granule position.
    s.encode.format = vorbis();
    let po = d.path().join("r.ogg");
    render_to_file(dc(RATE), &s, &FrozenClock, &po).unwrap();
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
    let mut render_cfg = config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO);
    render_cfg.render.sample_rate = tutti_types::SampleRate(96_000.0);
    render_cfg.render.duration_seconds = Seconds(1.0);
    let audio =
        render_to_buffers(dc(render_cfg.render.sample_rate), &render_cfg, &FrozenClock).unwrap();
    assert_eq!(audio.sample_rate().get(), 96_000.0);

    // ...then write with a config whose `render` half is left at its DEFAULT
    // 44100 — the case the doc tells a caller is fine to ignore.
    let mut write_cfg = config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO);
    write_cfg.resample = Some(Resample::to(tutti_types::SampleRate(48_000.0)));
    assert_eq!(
        write_cfg.render.sample_rate.get(),
        44_100.0,
        "this test is only meaningful while the config's render rate differs"
    );

    let p = d.path().join("w.wav");
    support::write_buffers(&audio, &write_cfg, &p).unwrap();

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
    use crate::support::render_normalized_to_file;
    use tutti_export::Normalize;
    use tutti_types::Db;

    let d = tempfile::tempdir().unwrap();
    // Longer than R128's 400 ms gating block, or nothing passes the gate.
    let mut cfg = config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO);
    cfg.render.duration_seconds = Seconds(2.0);

    let plain = d.path().join("plain.wav");
    render_to_file(dc(RATE), &cfg, &FrozenClock, &plain).unwrap();

    let normalized = d.path().join("normalized.wav");
    render_normalized_to_file(
        dc(RATE),
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
/// So the file must be re-measured, not merely re-read: for this signal the
/// two readings differ by ~1 dB. A 0.5 constant has a −6.02 dB *sample* peak but
/// a −4.99 dBTP *true* peak — the oversampling filter rings at the buffer's hard
/// leading edge. Asserting the sample peak against a dBTP target would be
/// comparing two different quantities, and would have to be loosened by exactly
/// the amount the feature is supposed to catch.
#[test]
fn peak_normalization_lands_on_the_requested_dbtp() {
    use crate::support::render_normalized_to_file;
    use tutti_export::Normalize;
    use tutti_measure::{LoudnessConfig, measure_loudness};
    use tutti_types::{Db, Interleaved};

    let d = tempfile::tempdir().unwrap();
    let mut cfg = config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO);
    cfg.render.duration_seconds = Seconds(2.0);

    let path = d.path().join("peak.wav");
    render_normalized_to_file(
        dc(RATE),
        &cfg,
        &FrozenClock,
        Normalize::peak(Db(-1.0)),
        &path,
    )
    .unwrap();

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

/// True peak, in dBTP, of a stereo WAV on disk (float or integer), measured
/// with the same R128 meter the export normalizes by.
fn file_true_peak(path: &std::path::Path) -> f32 {
    use tutti_measure::{LoudnessConfig, measure_loudness};
    use tutti_types::Interleaved;

    let reader = hound::WavReader::open(path).unwrap();
    let spec = reader.spec();
    let samples: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Float => reader.into_samples::<f32>().map(|s| s.unwrap()).collect(),
        hound::SampleFormat::Int => {
            let full = (1i64 << (spec.bits_per_sample - 1)) as f32;
            reader
                .into_samples::<i32>()
                .map(|s| s.unwrap() as f32 / full)
                .collect()
        }
    };
    let meter = LoudnessConfig::new(
        SampleRate(f64::from(spec.sample_rate)),
        ChannelLayout::STEREO,
    );
    measure_loudness(&meter, Interleaved::new(&samples, ChannelLayout::STEREO))
        .expect("stereo is measurable")
        .true_peak
        .get()
}

/// The trap `loudness.rs::a_reading_is_always_finite_even_below_the_gate` names,
/// reached through the export path: a render SHORTER than R128's 400 ms gating
/// block reports `-inf` LUFS, which the meter clamps to the -70 LUFS gate to
/// keep every gain finite. Policy (owner decision, lane E1): that floor is not
/// a measurement, and normalizing to a loudness from it would apply about
/// +47 dB to whatever the render holds. So a LUFS normalization of such a
/// render is `Unmeasurable { BelowGate }` and writes nothing, where it used to
/// write a finite file at that invented gain. A PEAK normalization of the same
/// render is measurable (a true peak needs no gate) and stays finite.
#[test]
fn a_sub_gating_block_render_cannot_be_loudness_normalized() {
    use crate::support::render_normalized_to_file;
    use tutti_export::{Normalize, Unmeasured};
    use tutti_types::Db;

    let d = tempfile::tempdir().unwrap();
    // 0.2 s — half the 400 ms gating block.
    let cfg = config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO);
    assert!(
        cfg.render.duration_seconds < Seconds(0.4),
        "this test is only meaningful under the gating block"
    );

    let path = d.path().join("short.wav");
    let err = render_normalized_to_file(
        dc(RATE),
        &cfg,
        &FrozenClock,
        Normalize::lufs(Db(-14.0)),
        &path,
    )
    .expect_err("nothing passed the gate");
    assert!(
        matches!(
            err,
            tutti_export::Error::Unmeasurable {
                reason: Unmeasured::BelowGate,
                output: Some(0),
                channels,
                sample_rate,
            } if channels == ChannelLayout::STEREO && sample_rate == RATE
        ),
        "{err:?}"
    );
    assert!(
        !path.exists(),
        "nothing is written for an unmeasurable render"
    );

    // The same render has a true peak, so a peak target is measurable, and
    // the gain it picks is finite.
    render_normalized_to_file(
        dc(RATE),
        &cfg,
        &FrozenClock,
        Normalize::peak(Db(-1.0)),
        &path,
    )
    .unwrap();
    let samples: Vec<f32> = hound::WavReader::open(&path)
        .unwrap()
        .into_samples::<f32>()
        .map(|s| s.unwrap())
        .collect();
    assert!(!samples.is_empty(), "a short render still writes frames");
    assert!(samples.iter().all(|s| s.is_finite()));
    let peak = file_true_peak(&path);
    assert!(
        (peak + 1.0).abs() < 0.05,
        "the short render lands on its -1 dBTP target, got {peak}"
    );
}

/// R128 meters any channel count, so a surround export normalizes against its
/// own loudness: it must not error, and must not silently switch to a
/// different metric.
#[test]
fn surround_normalizes_rather_than_falling_back_to_peak() {
    use crate::support::render_normalized_to_file;
    use tutti_export::Normalize;
    use tutti_types::Db;

    let d = tempfile::tempdir().unwrap();
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(6u16));
    let id = g.add(Const::frame(&[0.1, 0.2, 0.3, 0.4, 0.5, 0.6]));
    g.pipe_output(id);

    let mut cfg = config(Format::Wav(Pcm::Float32), ChannelLayout::from(6u16));
    cfg.render.duration_seconds = Seconds(2.0);

    let path = d.path().join("surround.wav");
    render_normalized_to_file(
        built(g, cfg.render.sample_rate),
        &cfg,
        &FrozenClock,
        Normalize::lufs(Db(-14.0)),
        &path,
    )
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

/// Silence cannot be normalized. Owner decision (lane E1): exact silence is
/// unmeasurable, so it is `Unmeasurable` (`Silent` for a peak target,
/// `BelowGate` for a loudness one) and nothing is written. It used to be
/// written as silence, which this test asserted (`peak <= 2 LSB`): the policy
/// moved from "safe by multiplying zero" to "refused", because the gain it was
/// multiplying (+144 dB, from the true-peak floor) was invented.
///
/// It still guards what the old test did, that `render_to_buffers` hands the
/// meter planes with no dither in them: dither added there makes the silence
/// read as ±1 LSB of signal, which is measurable, and the render succeeds.
///
/// (The planes `render_to_buffers` returns for silence are checked directly by
/// `silence_planes_carry_no_dither`.)
#[test]
fn normalizing_silence_is_refused() {
    use crate::support::render_normalized_to_file;
    use tutti_export::{Normalize, Unmeasured};
    use tutti_types::Db;

    let silence = || {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let id = g.add(Const::frame(&[0.0, 0.0]));
        g.pipe_output(id);
        built(g, RATE)
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("silence.wav");

    let mut cfg = config(Format::Wav(Pcm::Int16), ChannelLayout::STEREO);
    cfg.dither = Some(tutti_dsp::DitherShape::Triangular);

    for (normalize, want) in [
        (Normalize::peak(Db(-1.0)), Unmeasured::Silent),
        (Normalize::lufs(Db(-14.0)), Unmeasured::BelowGate),
    ] {
        let err = render_normalized_to_file(silence(), &cfg, &FrozenClock, normalize, &path)
            .expect_err("silence has no reading");
        assert!(
            matches!(&err, tutti_export::Error::Unmeasurable { reason, .. } if *reason == want),
            "{err:?}"
        );
        assert!(!path.exists());
    }
}

/// The dBTP target must hold in the written file, resample included.
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
    use crate::support::render_normalized_to_file;
    use tutti_export::Normalize;
    use tutti_measure::{LoudnessConfig, measure_loudness};
    use tutti_types::{Db, Interleaved};

    // Energy near Nyquist, with its true peak *between* samples, is what SRC
    // overshoots on; a DC constant barely moves and would hide the bug entirely.
    //
    // A sine at a quarter of the 44.1 kHz render rate, started at 0.546 turns
    // (a band-limited square at fs/4 keeps no harmonic below Nyquist but its
    // fundamental, so it renders the same). At that phase the samples are ±0.285 and ±0.958 of the peak
    // (±0.279 / ±0.939 at this amplitude of 0.98).
    //
    // The property that matters is where the meter looks. It estimates true
    // peak on a 4× oversampled grid, one point every 1/16 turn of this tone,
    // and from 0.546 turns the nearest grid point misses the crest by 0.0165
    // turns — so the render-rate estimate reads low, and a gain chosen from it
    // lands high once the resampled file puts samples closer to the crest.
    // At a start of 1/8 turn the grid lands exactly on the crest, the estimate
    // is already right, and the test passes with the measure-before-resample
    // bug put back. So the phase is load-bearing; the amplitude is not, since
    // peak normalization divides it out.
    let square = || {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
        let tone = Osc::sine(Hz(11025.0))
            .with_phase(tutti_types::Phase(0.546))
            .with_amplitude(Amplitude(0.98))
            .with_layout(ChannelLayout::STEREO);
        let id = g.add(tone);
        g.pipe_output(id);
        built(g, RATE)
    };

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("resampled.wav");

    let mut cfg = config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO);
    cfg.resample = Some(Resample::to(tutti_types::SampleRate(48_000.0)));

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

    let meter = LoudnessConfig::new(tutti_types::SampleRate(rate as f64), ChannelLayout::STEREO);
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
    use crate::support::render_normalized_to_file;
    use tutti_export::Normalize;
    use tutti_types::Db;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unmeasurable.wav");

    // R128 accepts 16 Hz - 2.8 MHz; 4 MHz is outside it.
    let mut cfg = config(Format::Wav(Pcm::Float32), ChannelLayout::STEREO);
    cfg.render.sample_rate = tutti_types::SampleRate(4_000_000.0);
    cfg.render.duration_seconds = Seconds(0.0005);

    let err = render_normalized_to_file(
        dc(cfg.render.sample_rate),
        &cfg,
        &FrozenClock,
        Normalize::peak(Db(-1.0)),
        &path,
    )
    .expect_err("an unmeasurable rate must not report success");

    assert!(
        matches!(err, tutti_export::Error::Unmeasurable { .. }),
        "expected Unmeasurable, got {err:?}"
    );
}

/// A master whose channel count is not one of 1/2/4/6/8/12 (3, 5, 7 or 9
/// wide) exports at that width: the width is runtime, and a caller passes
/// `ChannelLayout::from(master_width)` straight through.
///
/// Asserted through the file header and the samples, not the return value: "no
/// error" would also pass if the export quietly wrote a stereo file.
#[test]
fn an_unnamed_master_width_exports_at_that_width() {
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

        // A graph as wide as the file, carrying a distinct constant per channel so
        // a dropped or duplicated channel is visible.
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout);
        for c in 0..width as usize {
            let id = g.add(Const::mono(0.1 + 0.05 * c as f32));
            g.connect_output(id, 0, c);
        }

        let p = d.path().join(format!("w{width}.wav"));
        render_to_file(
            built(g, RATE),
            &config(Format::Wav(Pcm::Float32), layout),
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
    use support::write_buffers;

    let d = tempfile::tempdir().unwrap();
    for width in [3u16, 5] {
        let layout = ChannelLayout::from(width);
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, layout);
        for c in 0..width as usize {
            let id = g.add(Const::mono(0.25));
            g.connect_output(id, 0, c);
        }

        let cfg = config(Format::Wav(Pcm::Float32), layout);
        let rendered = render_to_buffers(built(g, RATE), &cfg, &FrozenClock)
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
/// that is rarely exercised would surface.
#[test]
fn an_odd_width_survives_a_resample() {
    let d = tempfile::tempdir().unwrap();
    let width = 5u16;
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(width));
    for c in 0..width as usize {
        let id = g.add(Const::mono(0.1 + 0.05 * c as f32));
        g.connect_output(id, 0, c);
    }

    let mut cfg = config(Format::Wav(Pcm::Float32), ChannelLayout::from(width));
    cfg.resample = Some(Resample::to(SampleRate::new(48_000.0)));

    let p = d.path().join("resampled5.wav");
    render_to_file(built(g, RATE), &cfg, &FrozenClock, &p).expect("a 5-wide resampled export");

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
        dc(RATE),
        &config(Format::Wav(Pcm::Float32), ChannelLayout::EMPTY),
        &FrozenClock,
        &p,
    )
    .expect_err("a zero-channel render must not report success");
    assert!(
        matches!(err, tutti_export::Error::UnsupportedChannels),
        "expected UnsupportedChannels, got {err:?}"
    );
}

/// A five-wide graph exported to a file narrower than it, where either width has
/// no channel order, is refused with `ConfigError::Placement` rather than cut to
/// its first channels; widened (or into an ordered file it folds into) it
/// renders.
#[test]
fn a_graph_with_no_place_in_the_file_is_refused() {
    use tutti_export::{ConfigError, Error};
    let five = || {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(5u16));
        let id = g.add(Const::new(0.25, ChannelLayout::from(5u16)));
        g.pipe_output(id);
        built(g, RATE)
    };
    for width in [1u16, 2, 3, 4] {
        let err = render_to_buffers(
            five(),
            &config(Format::Wav(Pcm::Float32), ChannelLayout::from(width)),
            &FrozenClock,
        );
        assert!(
            matches!(
                err,
                Err(Error::InvalidConfig(ConfigError::Placement(
                    tutti_types::FoldError::Unplaced { channel, .. }
                ))) if channel == usize::from(width)
            ),
            "five into {width}: {:?}",
            err.map(|_| ())
        );
    }
    for width in [5u16, 6, 8] {
        let ok = render_to_buffers(
            five(),
            &config(Format::Wav(Pcm::Float32), ChannelLayout::from(width)),
            &FrozenClock,
        );
        assert!(ok.is_ok(), "five into {width}");
    }
}

/// A four-wide graph whose channels are 0.1, 0.2, 0.3, 0.4.
fn four_buses(root: Option<ChannelTopology>) -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::QUAD);
    let id = g.add(Const::frame(&[0.1, 0.2, 0.3, 0.4]));
    g.pipe_output(id);
    built_with(g, RATE, Setup::new().with_root(root))
}

/// The first frame of every plane of a render.
fn first_frame(graph: &mut RenderGraph, width: u16) -> Result<Vec<f32>, tutti_export::Error> {
    let cfg = config(Format::Wav(Pcm::Float32), ChannelLayout::from(width));
    let out = render_to_buffers(graph, &cfg, &FrozenClock)?;
    Ok(out.planes().iter().map(|p| p[0]).collect())
}

/// A declared root is what the file is placed from: four `Unassigned` buses into
/// a six-channel file keep channels 2 and 3 at 2 and 3 (undeclared, a four is
/// quad and its rears land on the side pair at 4 and 5), a declared 5.1 folds
/// into stereo by the 5.1 rules, and a declaration the file cannot take, or
/// that is not as wide as the graph, is `ConfigError::Placement`.
#[test]
fn a_declared_root_is_what_the_file_is_placed_from() {
    use tutti_export::ConfigError;
    use tutti_types::{ChannelTopology, FoldError, Speaker};
    let mut graph = four_buses(None);
    assert_eq!(
        first_frame(&mut graph, 6).unwrap(),
        [0.1, 0.2, 0.0, 0.0, 0.3, 0.4]
    );
    let mut graph = four_buses(Some(ChannelTopology::unassigned(ChannelLayout::QUAD)));
    assert_eq!(
        first_frame(&mut graph, 6).unwrap(),
        [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]
    );
    // Four buses cannot be placed in a stereo file.
    assert!(matches!(
        first_frame(&mut graph, 2),
        Err(tutti_export::Error::InvalidConfig(ConfigError::Placement(
            FoldError::Unplaced { channel: 2, .. }
        )))
    ));
    // A declaration of another width than the graph is refused.
    let mut graph = four_buses(Some(ChannelTopology::unassigned(ChannelLayout::from(6u16))));
    assert!(matches!(
        first_frame(&mut graph, 6),
        Err(tutti_export::Error::InvalidConfig(ConfigError::Placement(
            FoldError::RootWidth { .. }
        )))
    ));
    // Declared as a 5.1 (six wide), a six-channel graph folds to stereo by it.
    let six_channels = |root: ChannelTopology| {
        let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::from(6u16));
        let id = g.add(Const::frame(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]));
        g.pipe_output(id);
        built_with(g, RATE, Setup::new().with_root(Some(root)))
    };
    let swapped = ChannelTopology::smpte(ChannelLayout::from(6u16)).unwrap();
    assert!(swapped.index_of(Speaker::FrontLeft).is_some());
    let mut six = six_channels(swapped);
    let m = tutti_types::M3DB;
    let stereo = first_frame(&mut six, 2).unwrap();
    assert_eq!(stereo, [1.0 + m * 3.0 + m * 5.0, 2.0 + m * 3.0 + m * 6.0]);
    // The same six, the front pair declared the other way round.
    let swapped = ChannelTopology::new([
        Speaker::FrontRight,
        Speaker::FrontLeft,
        Speaker::FrontCenter,
        Speaker::LowFrequency,
        Speaker::SideLeft,
        Speaker::SideRight,
    ]);
    let mut six = six_channels(swapped);
    assert_eq!(
        first_frame(&mut six, 2).unwrap(),
        [2.0 + m * 3.0 + m * 5.0, 1.0 + m * 3.0 + m * 6.0]
    );
}

/// `render_to_buffers` of silence gives exactly 0.0 at an integer depth with
/// dither on: dither belongs to the encode boundary, not to the planes a meter
/// reads.
#[test]
fn silence_planes_carry_no_dither() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let id = g.add(Const::frame(&[0.0, 0.0]));
    g.pipe_output(id);
    let mut cfg = config(Format::Wav(Pcm::Int16), ChannelLayout::STEREO);
    cfg.dither = Some(tutti_dsp::DitherShape::Triangular);
    let out = render_to_buffers(built(g, RATE), &cfg, &FrozenClock).unwrap();
    assert!(out.frames().get() > 0);
    assert!(out.planes().iter().flatten().all(|s| s.to_bits() == 0));
}

/// Rings 4000 frames at most (its ceiling), and reports 100 each block.
#[derive(Clone)]
struct Narrowing;

impl Node for Narrowing {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
            .with_tail(tutti_types::Tail::Finite(tutti_types::Samples(4_000)))
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        io.report_tail(tutti_types::Tail::Finite(tutti_types::Samples(100)));
        let (ins, mut outs) = io.split();
        outs.get(0).copy_from_slice(ins.get(0));
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// **Export spends a node's ceiling, not what it reports each block** (doc
/// 022 section 12.4's fallback). `RenderConfig::tail` is a frame count fixed
/// before the render (`RenderPlan::new` sizes `output_length` and `total`
/// from it, and the sink, the progress total, the resampler and the
/// normalized two-pass render all read it), so a tail the graph reports
/// only at the programme's end has nowhere to go without a second phase in
/// all of them. Until that lands the figure is the spec's: a node that
/// narrows its tail renders the longer, safe ceiling.
#[test]
fn export_spends_the_ceiling_of_a_node_that_narrows_its_tail() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::MONO);
    let src = g.add(Const::mono(0.5));
    let node = g.add(ForkByClone(Narrowing));
    g.connect(src, 0, node, 0).pipe_output(node);
    let graph = built(g, RATE);

    let reported = graph.reported_tail();
    assert_eq!(reported.samples(), Some(tutti_types::Samples(4_000)));

    let mut cfg = config(Format::Wav(Pcm::Float32), ChannelLayout::MONO);
    cfg.render.tail = reported.samples().expect("finite");
    let out = render_to_buffers(graph, &cfg, &FrozenClock).unwrap();
    let duration = 8_820; // 0.2 s at 44.1 kHz
    assert_eq!(out.frames().get(), duration + 4_000);
}
