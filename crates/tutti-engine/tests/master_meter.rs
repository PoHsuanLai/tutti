//! The engine feeds the master meter, whatever renders it: the level of the
//! block just rendered, after the declick, on a stereo fold of the device
//! buffer. These moved here from `tutti-cpal`'s output-block tests, where the
//! callback fed the meter.

use tutti_engine::Engine;
use tutti_graph::{Editor, Prepare};
use tutti_transport::Transport;
use tutti_types::graph::{OutPort, Source};
use tutti_types::{ChannelLayout, InterleavedMut, M3DB, NodeKey, SampleRate, Samples};

const RATE: f64 = 48_000.0;

/// A mono source writing a constant.
struct Dc(f32);

impl tutti_graph::Node for Dc {
    fn shape(&self) -> tutti_graph::Shape {
        tutti_graph::Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(
        &mut self,
        _: &tutti_graph::Cx<'_>,
        mut io: tutti_graph::Io<'_>,
    ) -> tutti_graph::Status {
        io.output(0).fill(self.0);
        tutti_graph::Status::Modified
    }
    fn reset(&mut self) {}
}

/// An engine over a constant `level` on output `channel` of `outputs` (silence
/// on the rest), or on every output when `channel` is `None`.
fn engine(level: f32, channel: Option<usize>, outputs: usize, capacity: Samples) -> Engine {
    let transport = Transport::new(RATE);
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(RATE), Samples(512)));
    ed.insert(NodeKey(1), "dc", tutti_graph::Unforkable(Dc(level)));
    ed.spec_mut().topology.outputs = (0..outputs)
        .map(|c| {
            if channel.is_none_or(|only| only == c) {
                Source::Node(OutPort {
                    node: NodeKey(1),
                    port: 0,
                })
            } else {
                Source::Zero
            }
        })
        .collect();
    ed.commit().expect("commits");
    let engine = Engine::with_capacity(&transport, &mut ed, exec, capacity).expect("within limits");
    // The engine's editor half outlives the test; nothing commits again.
    Box::leak(Box::new((ed, transport)));
    engine
}

fn render(engine: &Engine, frames: usize, width: usize) -> Vec<f32> {
    let mut out = vec![0.0f32; frames * width];
    engine.process(&mut InterleavedMut::new(
        &mut out,
        ChannelLayout::from(width),
    ));
    out
}

/// **A wide device meters a fold of every channel, not the first two.**
///
/// A 5.1 mix whose dialogue (the centre) never appears on the meter would be
/// the bug. The signal is put on the **centre** channel with the front pair
/// silent, which is the only arrangement that separates a fold from a `[..2]`
/// truncation: under a uniform-DC graph the two agree exactly. The fold of a
/// centre alone is `M3DB` on each side.
///
/// Mutation (run, recorded here): measure `&output[..frames * 2]` of the wide
/// buffer instead of folding (the `[..2]` truncation) -> the meter reads 0
/// and this fails.
#[test]
fn a_wide_device_meters_a_fold_not_the_first_two_channels() {
    // 5.1 order: L, R, C, LFE, Ls, Rs. Channel 2 is the centre.
    let engine = engine(1.0, Some(2), 6, Samples(512));
    engine.master_meter().enable();
    let out = render(&engine, 64, 6);

    // The device buffer itself: silent front pair, loud centre.
    assert_eq!(out[0], 0.0, "left must be silent");
    assert_eq!(out[1], 0.0, "right must be silent");
    assert_eq!(out[2], 1.0, "the centre carries the signal");

    let r = engine.master_meter().get();
    assert!(
        r.peak_left.get() != 0.0 || r.peak_right.get() != 0.0,
        "the metered fold must carry the centre channel; it read ({}, {}), \
         which is what a `[..2]` truncation of this buffer would give",
        r.peak_left.get(),
        r.peak_right.get()
    );
    assert!(
        (r.peak_left.get() - M3DB).abs() < 1e-6,
        "centre folds at -3 dB"
    );
    assert!((r.peak_right.get() - M3DB).abs() < 1e-6);
}

/// **The meter measures only while enabled.** A disabled meter reads nothing
/// of a rendered block; once enabled it reads the next one. A stereo device
/// (measured as it is) and a wide one (folded) are both checked.
///
/// Mutation (run, recorded here): drop the `is_enabled` early return in
/// `Heard::measure` -> the disabled meter reads 0.5 and the first assertion
/// fails.
#[test]
fn an_enabled_meter_reads_the_block() {
    for width in [2usize, 6] {
        let engine = engine(0.5, None, width, Samples(512));
        render(&engine, 64, width);
        assert_eq!(
            engine.master_meter().get().peak_left.get(),
            0.0,
            "width {width}: disabled reads nothing"
        );

        engine.master_meter().enable();
        render(&engine, 64, width);
        let peak = engine.master_meter().get().peak_left.get();
        assert!(peak > 0.0, "width {width}: the enabled meter read {peak}");
    }
}

/// **A stereo block at the `MAX_FRAMES` ceiling (8192) reads its analytic
/// level**: a constant 0.5 reads 0.5 as both peak and RMS. (A wide device's
/// fold is not a passthrough, so its figure is the fold's, pinned by
/// `a_wide_device_meters_a_fold_not_the_first_two_channels`.)
///
/// Mutation (run, recorded here): delete the `measure` call at the end of
/// `Engine::process` -> the block reads 0.0.
#[test]
fn a_full_block_reads_its_analytic_level() {
    let engine = engine(0.5, None, 2, Samples(8192));
    engine.master_meter().enable();
    render(&engine, 8192, 2);
    let r = engine.master_meter().get();
    for (what, got) in [
        ("peak left", r.peak_left),
        ("peak right", r.peak_right),
        ("rms left", r.rms_left),
        ("rms right", r.rms_right),
    ] {
        assert!((got.get() - 0.5).abs() < 1e-6, "{what} read {}", got.get());
    }
}

/// **The pure render feeds the meter too**: `process_segment` has no declick
/// but is a render a host pulls, and an offline session's handle must not
/// read silence for it.
///
/// Mutation (run, recorded here): delete the `measure` call at the end of
/// `process_segment` -> the meter reads 0.0.
#[test]
fn the_pure_render_feeds_the_meter() {
    let engine = engine(0.25, None, 2, Samples(512));
    engine.master_meter().enable();
    let mut out = vec![0.0f32; 64 * 2];
    engine.process_segment(&mut InterleavedMut::new(&mut out, ChannelLayout::STEREO));
    assert!((engine.master_meter().get().peak_left.get() - 0.25).abs() < 1e-6);
}

/// A wide block longer than the fold's scratch is folded and measured piece
/// by piece, not dropped or overrun: the meter reads the last piece, which
/// here carries the same centre figure. The scratch holds the block capacity
/// (the larger of 512, the prepared block, and what is asked for), so the
/// block is longer than that.
///
/// Mutation (run, recorded here): `output.chunks(piece * channels)` ->
/// `output.chunks(output.len())` -> the fold slice indexes past the scratch and the
/// test panics.
#[test]
fn a_block_longer_than_the_fold_scratch_is_measured_in_pieces() {
    let engine = engine(1.0, Some(2), 6, Samples(64));
    engine.master_meter().enable();
    render(&engine, 1500, 6);
    let r = engine.master_meter().get();
    assert!((r.peak_left.get() - M3DB).abs() < 1e-6, "{r:?}");
}

/// **The stereo measuring shortcut agrees with the fold.**
///
/// `Heard::measure` measures a stereo buffer as it is, which is only the same
/// as folding it while `fold_frame`'s 2-wide arm is a passthrough. This pins
/// that premise; it cannot detect the shortcut being *deleted* (both paths
/// then agree: a performance change, not a correctness one).
#[test]
fn the_stereo_metering_shortcut_agrees_with_the_fold() {
    let mut folded = [0.0f32; 2];
    tutti_types::fold_frame(&[0.25, -0.75], &mut folded);
    assert_eq!(
        folded,
        [0.25, -0.75],
        "fold_frame's 2-wide arm must stay a passthrough: the engine's stereo \
         branch is only valid while it is. If this fails, delete that branch \
         rather than patching it."
    );
}
