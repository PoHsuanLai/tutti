//! One render pass feeding several outputs (doc 019 section 4, step 3): files
//! of different formats, plan taps, a gain shared between files, a rename
//! that fails half way, and the outputs that contradict each other.
//!
//! Every expectation is derived from the signal, not from another render: the
//! planes of a [`BufferTarget`] output of the same pass are the audio the
//! files are checked against (through each format's own quantizer), a tap on a
//! node is checked against the figure its gain makes of the node's constant,
//! and files that share a gain are checked against the relation that gain
//! implies (the stems sum to the master).

#![cfg(all(feature = "wav", feature = "flac"))]

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use tutti_export::{
    BufferTarget, ChannelLayout, Control, Error, FileTarget, FlacLevel, Format, FrozenClock,
    IntDepth, Job, Normalize, NormalizeSpec, Output, Pcm, Placement, Produced, RenderGraph,
    TargetError, render,
};
use tutti_graph::{
    Cx, ForkByClone, GraphBuilder, Io, Node, NodeKey, OutPort, Prepare, Shape, Source, Status,
    TapAlign, TapId, TapReader,
};
use tutti_nodes::testing::{Const, Osc};
use tutti_types::{Amplitude, ChannelTopology, Db, Hz, SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);

/// A stereo node that scales its input by a constant: the known gain.
#[derive(Clone)]
struct Scale(f32);

impl Node for Scale {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::STEREO, ChannelLayout::STEREO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let (ins, mut outs) = io.split();
        for c in 0..2 {
            for (o, i) in outs.get(c).iter_mut().zip(ins.get(c)) {
                *o = *i * self.0;
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn stereo_out(g: &mut GraphBuilder, node: NodeKey) {
    g.connect_output(node, 0, 0).connect_output(node, 1, 1);
}

fn finish(g: GraphBuilder) -> RenderGraph {
    let (editor, executor) = g.build(RenderGraph::prepare(RATE)).expect("builds");
    RenderGraph::new(editor, executor).expect("built together")
}

/// A tap on both channels of `node`, on the outputs' time, committed.
fn tap(graph: &mut RenderGraph, id: u32, node: NodeKey) -> TapReader {
    let sources = (0..2)
        .map(|port| Source::Node(OutPort { node, port }))
        .collect();
    let reader = graph
        .editor_mut()
        .open_tap(TapId(id), sources, TapAlign::ToOutput, Samples(2048))
        .expect("the node exists");
    graph.commit().expect("commits");
    reader
}

fn stereo_placement() -> Placement {
    Placement::new(ChannelTopology::for_width(ChannelLayout::STEREO))
}

fn job(frames: usize) -> Job {
    Job::new(RATE, Samples(frames), Samples(0), Samples(0)).expect("a render")
}

fn wav(path: &std::path::Path, pcm: Pcm) -> FileTarget {
    FileTarget::new(path, Format::Wav(pcm), ChannelLayout::STEREO).with_dither(None)
}

fn flac(path: &std::path::Path, depth: IntDepth) -> FileTarget {
    FileTarget::new(
        path,
        Format::Flac {
            depth,
            level: FlacLevel::default(),
        },
        ChannelLayout::STEREO,
    )
    .with_dither(None)
}

fn planes(produced: Produced) -> Vec<Vec<f32>> {
    produced.buffers().expect("a buffers output").into_planes()
}

/// Decodes a file with `symphonia`, which shares no code with the encoders,
/// into one plane per channel.
fn decode(path: &std::path::Path) -> Vec<Vec<f32>> {
    let file = std::fs::File::open(path).expect("open");
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    hint.with_extension(
        path.extension()
            .and_then(|e| e.to_str())
            .expect("an extension"),
    );
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
    let channels = track.codec_params.channels.expect("channels").count();
    let mut planes = vec![Vec::new(); channels];
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track.id {
            continue;
        }
        if let Ok(audio) = decoder.decode(&packet) {
            let mut buf = SampleBuffer::<f32>::new(audio.capacity() as u64, *audio.spec());
            buf.copy_interleaved_ref(audio);
            for frame in buf.samples().chunks(channels) {
                for (plane, s) in planes.iter_mut().zip(frame) {
                    plane.push(*s);
                }
            }
        }
    }
    planes
}

/// A decoded plane as integers at `depth`: the float symphonia gives back is
/// exactly `integer / 2^(bits - 1)` for a lossless 16 or 24 bit file.
fn as_ints(plane: &[f32], depth: IntDepth) -> Vec<i32> {
    let scale = f64::from(1u32 << (depth.bits() - 1));
    plane
        .iter()
        .map(|s| (f64::from(*s) * scale).round() as i32)
        .collect()
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

/// Two tones, one per channel, at different pitches and levels, so a swapped
/// channel or a misplaced frame reads as a different number.
fn two_tones() -> RenderGraph {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let left = g.add(Osc::sine(Hz(440.0)).with_amplitude(Amplitude(0.5)));
    let right = g.add(Osc::sine(Hz(660.0)).with_amplitude(Amplitude(0.4)));
    g.connect_output(left, 0, 0).connect_output(right, 0, 1);
    finish(g)
}

/// One pass writes a 24-bit WAV and a 16-bit FLAC, and each file is its own
/// quantization of the audio the pass rendered: the planes a third output
/// holds in memory, put through each format's quantizer, are exactly what each
/// file decodes to. A different quantization per file is what a pass that
/// fed one encoder's output to the other would not give.
///
/// The graph is stepped once for the three outputs: a timeline clock, which
/// advances once per block rendered, stands exactly the render's frames on.
#[test]
fn two_outputs_of_different_formats_are_each_their_own_quantization_of_one_pass() {
    use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};
    use tutti_types::{Beat, Bpm};

    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.wav"), dir.path().join("b.flac"));
    let frames = 9_600;
    let timeline = OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo: Bpm::of(120.0),
        sample_rate: RATE,
        loop_range: None,
    });
    let produced = render(
        &mut two_tones(),
        &job(frames),
        &timeline,
        vec![
            Output::master(wav(&a, Pcm::Int24)),
            Output::master(flac(&b, IntDepth::Int16)),
            Output::master(BufferTarget::new(ChannelLayout::STEREO)),
        ],
        &mut Control::none(),
    )
    .expect("renders")
    .finished()
    .expect("not cancelled");
    assert_eq!(produced.len(), 3);
    let mut produced = produced.into_iter();
    let (wa, wb) = (
        produced.next().unwrap().file().unwrap(),
        produced.next().unwrap().file().unwrap(),
    );
    assert_eq!((wa.path.as_path(), wb.path.as_path()), (&*a, &*b));
    let held = planes(produced.next().unwrap());
    assert_eq!(held[0].len(), frames);
    let peak = held[0].iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!(peak > 0.45, "the tone is there: {peak}");

    for (path, depth) in [(&a, IntDepth::Int24), (&b, IntDepth::Int16)] {
        let decoded = decode(path);
        for (channel, plane) in decoded.iter().enumerate() {
            let want: Vec<i32> = held[channel].iter().map(|s| depth.quantize(*s)).collect();
            assert_eq!(
                as_ints(plane, depth),
                want,
                "{}: channel {channel} is the held plane through the {depth:?} quantizer",
                path.display()
            );
        }
    }

    let stood = (timeline.beat().get() / timeline.beats_per_sample().get()).round() as usize;
    assert_eq!(
        stood, frames,
        "the graph was stepped once, not once per output"
    );
    assert_eq!(names(dir.path()), ["a.wav", "b.flac"], "no temporary left");
}

/// Stereo `0.5` through a `0.5` gain, on the master, with a tap on each of the
/// source (before the gain) and the gain (after it).
fn gain_graph() -> (RenderGraph, NodeKey, NodeKey) {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let source = g.add(Const::frame(&[0.5, 0.5]));
    let gain = g.add(ForkByClone(Scale(0.5)));
    g.pipe(source, gain);
    stereo_out(&mut g, gain);
    (finish(g), source, gain)
}

/// A tap on a node's port renders that node's own signal, not the master's:
/// a constant `0.5` read before a `0.5` gain is `0.5`, read after it `0.25`,
/// and the master (after the gain) is `0.25`. The three outputs come from one
/// pass.
#[test]
fn a_tap_renders_the_signal_of_the_node_it_is_on() {
    let (mut graph, source, gain) = gain_graph();
    let before = tap(&mut graph, 1, source);
    let after = tap(&mut graph, 2, gain);
    let produced = render(
        &mut graph,
        &job(3_000),
        &FrozenClock,
        vec![
            Output::master(BufferTarget::new(ChannelLayout::STEREO)),
            Output::tap(
                before,
                stereo_placement(),
                BufferTarget::new(ChannelLayout::STEREO),
            ),
            Output::tap(
                after,
                stereo_placement(),
                BufferTarget::new(ChannelLayout::STEREO),
            ),
        ],
        &mut Control::none(),
    )
    .expect("renders")
    .finished()
    .expect("not cancelled");
    let [master, before, after]: [Vec<Vec<f32>>; 3] = produced
        .into_iter()
        .map(planes)
        .collect::<Vec<_>>()
        .try_into()
        .expect("three outputs");
    for (name, got, want) in [
        ("master", master, 0.25),
        ("before the gain", before, 0.5),
        ("after the gain", after, 0.25),
    ] {
        assert_eq!(got.len(), 2, "{name}");
        for (c, plane) in got.iter().enumerate() {
            assert_eq!(plane.len(), 3_000, "{name}");
            assert!(plane.iter().all(|s| *s == want), "{name} channel {c}");
        }
    }
}

/// A tap shares the master's gate: with a latency trim of more than a block
/// the first block is dropped whole, its tap frames are still drained, and the
/// file starts where the master would: at frame 1500 of a sine.
///
/// The sine is `0.4 sin(2 pi 440 n / 48000)` through a `0.5` gain, so the tap
/// on the gain reads `0.2 sin(...)`; its first kept frame is frame 1500 of it.
#[test]
fn a_tap_is_drained_and_gated_like_the_master() {
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let osc = g.add(
        Osc::sine(Hz(440.0))
            .with_amplitude(Amplitude(0.4))
            .with_layout(ChannelLayout::STEREO),
    );
    let gain = g.add(ForkByClone(Scale(0.5)));
    g.pipe(osc, gain);
    stereo_out(&mut g, gain);
    let mut graph = finish(g);
    let reader = tap(&mut graph, 1, gain);
    let job = Job::new(RATE, Samples(2_000), Samples(1_500), Samples(0)).expect("a render");
    let produced = render(
        &mut graph,
        &job,
        &FrozenClock,
        vec![Output::tap(
            reader,
            stereo_placement(),
            BufferTarget::new(ChannelLayout::STEREO),
        )],
        &mut Control::none(),
    )
    .expect("renders")
    .finished()
    .expect("not cancelled");
    let got = planes(produced.into_iter().next().unwrap());
    assert_eq!(got[0].len(), 2_000);
    for (i, s) in got[0].iter().enumerate() {
        let n = (1_500 + i) as f64;
        let want = 0.2 * (std::f64::consts::TAU * 440.0 * n / 48_000.0).sin();
        assert!(
            (f64::from(*s) - want).abs() < 2e-4,
            "frame {i}: {s} vs {want}"
        );
    }
}

/// A tap whose lanes cannot be placed on its target, and a tap that is not as
/// wide as its placement, are named errors, found before anything is rendered
/// and leaving no file.
#[test]
fn a_tap_that_cannot_be_placed_is_a_named_error() {
    let dir = tempfile::tempdir().unwrap();
    let (mut graph, source, gain) = gain_graph();
    let wide = tap(&mut graph, 1, source);
    let narrow = tap(&mut graph, 2, gain);

    // Two lanes, a placement of one.
    let err = render(
        &mut graph,
        &job(1_000),
        &FrozenClock,
        vec![Output::tap(
            wide,
            Placement::new(ChannelTopology::for_width(ChannelLayout::MONO)),
            wav(&dir.path().join("a.wav"), Pcm::Int16),
        )],
        &mut Control::none(),
    )
    .expect_err("two lanes are not one");
    assert!(
        matches!(
            err,
            Error::Target {
                output: 0,
                why: TargetError::Lanes { lanes, placement }
            } if lanes == ChannelLayout::STEREO && placement == ChannelLayout::MONO
        ),
        "{err:?}"
    );

    // The same check at the second output: it is made for every output before
    // the first file is begun, so the master's is never created.
    let err = render(
        &mut graph,
        &job(1_000),
        &FrozenClock,
        vec![
            Output::master(wav(&dir.path().join("m.wav"), Pcm::Int16)),
            Output::tap(
                narrow,
                Placement::new(ChannelTopology::discrete(ChannelLayout::from(3u16))),
                wav(&dir.path().join("b.wav"), Pcm::Int16),
            ),
        ],
        &mut Control::none(),
    )
    .expect_err("two lanes are not three");
    assert!(matches!(
        err,
        Error::Target {
            output: 1,
            why: TargetError::Lanes { .. }
        }
    ));
    assert!(names(dir.path()).is_empty(), "nothing was left behind");
}

/// A placement that is as wide as its tap but cannot be folded onto the
/// target is `TargetError::Placement`, and the first output's file, already
/// begun by then, is removed.
#[test]
fn a_placement_the_target_cannot_take_is_a_target_error_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut graph, source, _) = gain_graph();
    let reader = tap(&mut graph, 1, source);
    let err = render(
        &mut graph,
        &job(1_000),
        &FrozenClock,
        vec![
            Output::master(wav(&dir.path().join("m.wav"), Pcm::Int16)),
            Output::tap(
                reader,
                // Two discrete feeds onto a mono file: discrete channels are
                // not downmixed, so there is no place for them.
                Placement::new(ChannelTopology::discrete(ChannelLayout::STEREO)),
                FileTarget::new(
                    dir.path().join("t.wav"),
                    Format::Wav(Pcm::Int16),
                    ChannelLayout::MONO,
                ),
            ),
        ],
        &mut Control::none(),
    )
    .expect_err("discrete feeds are not L and R");
    assert!(
        matches!(
            err,
            Error::Target {
                output: 1,
                why: TargetError::Placement(_)
            }
        ),
        "{err:?}"
    );
    assert!(names(dir.path()).is_empty(), "the first file was removed");
}

/// Three files from one pass: a master and two stems that sum to it, the
/// master normalized and the stems taking its gain. All three report the same
/// gain, the stems' files are the stems scaled by it, and they still add up to
/// the master's: the balance the shared gain exists to keep.
///
/// The gain is the master's: `Written::gain` is the peak target less the
/// master's measured true peak, and a stem has no reading of its own.
#[test]
fn stems_that_take_the_masters_gain_still_sum_to_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut g = GraphBuilder::new(ChannelLayout::EMPTY, ChannelLayout::STEREO);
    let a = g.add(Const::frame(&[0.2, 0.1]));
    let b = g.add(Const::frame(&[0.1, 0.25]));
    stereo_out(&mut g, a);
    stereo_out(&mut g, b);
    let mut graph = finish(g);
    let (ta, tb) = (tap(&mut graph, 1, a), tap(&mut graph, 2, b));

    let mode = Normalize::peak(Db(-6.0));
    let files: Vec<std::path::PathBuf> = ["master.wav", "a.wav", "b.wav"]
        .iter()
        .map(|n| dir.path().join(n))
        .collect();
    let float = |path: &std::path::Path, spec| {
        FileTarget::new(path, Format::Wav(Pcm::Float32), ChannelLayout::STEREO)
            .with_normalize(Some(spec))
    };
    let mut produced = render(
        &mut graph,
        &job(24_000),
        &FrozenClock,
        vec![
            Output::master(float(&files[0], NormalizeSpec::own(mode))),
            Output::tap(
                ta,
                stereo_placement(),
                float(&files[1], NormalizeSpec::from_output(mode, 0)),
            ),
            Output::tap(
                tb,
                stereo_placement(),
                float(&files[2], NormalizeSpec::from_output(mode, 0)),
            ),
        ],
        &mut Control::none(),
    )
    .expect("renders")
    .finished()
    .expect("not cancelled")
    .into_iter()
    .map(|p| p.file().expect("files"));
    let (master, sa, sb) = (
        produced.next().unwrap(),
        produced.next().unwrap(),
        produced.next().unwrap(),
    );

    let gain = master.gain.expect("the master normalized");
    let reading = master.measured.expect("and measured itself");
    assert!(
        (gain.get() - (-6.0 - reading.true_peak.get())).abs() < 1e-5,
        "the gain is the target less the true peak: {gain:?} for {reading:?}"
    );
    assert_eq!((sa.gain, sb.gain), (Some(gain), Some(gain)));
    assert_eq!(
        (sa.measured, sb.measured),
        (None, None),
        "they measured nothing"
    );

    let scale = gain.to_amplitude().get();
    let (m, da, db) = (decode(&files[0]), decode(&files[1]), decode(&files[2]));
    for c in 0..2 {
        let want_a = [0.2, 0.1][c] * scale;
        let want_b = [0.1, 0.25][c] * scale;
        for i in 0..24_000 {
            assert!(
                (da[c][i] - want_a).abs() < 1e-6,
                "a, channel {c}, frame {i}"
            );
            assert!(
                (db[c][i] - want_b).abs() < 1e-6,
                "b, channel {c}, frame {i}"
            );
            assert!(
                (da[c][i] + db[c][i] - m[c][i]).abs() < 1e-6,
                "the stems sum to the master: channel {c}, frame {i}"
            );
        }
    }
}

/// A rename that fails after another landed is `NotCommitted`, naming the file
/// that landed, which is kept; the file that did not is not left behind and
/// what was in its way is as it was. A first rename that fails is the plain
/// error, with nothing landed.
///
/// The second target is a directory, which a file cannot replace.
#[test]
fn a_rename_that_fails_after_another_landed_is_not_committed() {
    let dir = tempfile::tempdir().unwrap();
    let (first, second) = (dir.path().join("first.wav"), dir.path().join("second.wav"));
    std::fs::write(&first, b"old").unwrap();
    std::fs::create_dir(&second).unwrap();
    std::fs::write(second.join("keep"), b"x").unwrap();

    let err = render(
        &mut two_tones(),
        &job(2_000),
        &FrozenClock,
        vec![
            Output::master(wav(&first, Pcm::Int16)),
            Output::master(wav(&second, Pcm::Int16)),
        ],
        &mut Control::none(),
    )
    .expect_err("a file cannot replace a directory");
    match err {
        Error::NotCommitted { landed, cause } => {
            assert_eq!(landed, std::slice::from_ref(&first));
            assert!(matches!(cause, tutti_codec::Error::Io(_)), "{cause:?}");
        }
        other => panic!("{other:?}"),
    }
    let new = std::fs::read(&first).unwrap();
    assert!(
        new.starts_with(b"RIFF"),
        "the first file landed and is kept"
    );
    assert!(
        second.join("keep").exists(),
        "what was in the way is as it was"
    );
    assert_eq!(
        names(dir.path()),
        ["first.wav", "second.wav"],
        "no temporary"
    );

    // The first rename failing lands nothing: the plain error.
    std::fs::remove_file(&first).unwrap();
    std::fs::create_dir(&first).unwrap();
    let err = render(
        &mut two_tones(),
        &job(2_000),
        &FrozenClock,
        vec![
            Output::master(wav(&first, Pcm::Int16)),
            Output::master(wav(&dir.path().join("third.wav"), Pcm::Int16)),
        ],
        &mut Control::none(),
    )
    .expect_err("a file cannot replace a directory");
    assert!(matches!(err, Error::Io(_)), "{err:?}");
    assert!(!dir.path().join("third.wav").exists());
}

/// Outputs that contradict each other are named errors before anything is
/// rendered: none at all, a gain taken from the output itself, from one past
/// the list or from one that does not measure itself, and two files at one
/// path.
#[test]
fn outputs_that_contradict_each_other_are_named_errors() {
    let dir = tempfile::tempdir().unwrap();
    let mode = Normalize::peak(Db(-6.0));
    let file = |n: &str, spec: Option<NormalizeSpec>| {
        Output::master(wav(&dir.path().join(n), Pcm::Int16).with_normalize(spec))
    };
    let run = |outputs: Vec<Output>| {
        render(
            &mut two_tones(),
            &job(2_000),
            &FrozenClock,
            outputs,
            &mut Control::none(),
        )
    };

    assert!(matches!(
        run(vec![]).expect_err("nothing to render"),
        Error::NoOutputs
    ));
    for (outputs, bad) in [
        (
            vec![file("a.wav", Some(NormalizeSpec::from_output(mode, 0)))],
            0,
        ),
        (
            vec![file("a.wav", Some(NormalizeSpec::from_output(mode, 4)))],
            4,
        ),
        (
            vec![
                file("a.wav", None),
                file("b.wav", Some(NormalizeSpec::from_output(mode, 0))),
            ],
            0,
        ),
    ] {
        let err = run(outputs).expect_err("a gain from nowhere");
        assert!(
            matches!(
                err,
                Error::Target { why: TargetError::GainSource { index }, .. } if index == bad
            ),
            "{err:?}"
        );
    }
    let err = run(vec![file("a.wav", None), file("a.wav", None)]).expect_err("one path twice");
    assert!(
        matches!(
            err,
            Error::Target {
                output: 1,
                why: TargetError::SamePath { other: 0 }
            }
        ),
        "{err:?}"
    );
    assert!(names(dir.path()).is_empty());
}
