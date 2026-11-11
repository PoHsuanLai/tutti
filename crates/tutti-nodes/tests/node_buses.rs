//! A node whose input and output widths differ says where its channels sit,
//! so the graph's dry path (the mix, bypass) folds the input onto the outputs
//! by speaker, not by index (doc 013, "Channel placement").
//!
//! The graph passes a node that does not run through its main buses
//! (`Pass::of`): two placed buses fold by `Fold::between` (a mono centre
//! reaches both sides of a stereo pair at gain 1), and buses of `Discrete`
//! channels, which `Shape::audio` declares from a width, pass output `c` =
//! input `c`. For a node that keeps its width that is the input; for a mono to
//! stereo reverb it put the dry signal on the left alone. So every node type
//! in the crate is in the table below: built at the widths it changes, and
//! either placed or by index for a stated reason.

use std::sync::Arc;

use tutti_curve::Trajectory;
use tutti_graph::{Node, Shape};
use tutti_modulation::{ControlProgram, SourceId};
use tutti_nodes::automation::{AutomationLaneNode, ControlLaneNode};
use tutti_nodes::{
    BrickwallLimiterNode, BusStripNode, ClickNode, ClickSettings, CompressorNode, CrossoverNode,
    DelayLineNode, DesignNode, DistortionNode, EnvelopeFollowerNode, EqBandNode, FdnControls,
    FdnDesign, FreeverbNode, GateNode, LfoNode, LfoShape, LimiterNode, ModDelayConfig,
    ModDelayNode, MultibandCompressorNode, NoteEnvelopeNode, PhaserNode, ShapeKind, SvfFilterNode,
    SvfType, VbapPannerNode,
};
use tutti_transport::Transport;
use tutti_types::{
    Adsr, ChannelLayout, CompressionRatio, Db, Drive, Feedback, Hz, Q, SampleRate, Seconds, Speaker,
};

/// What the graph's dry path does with a node's channels.
enum Dry {
    /// Its main buses are speakers where its width changes: the dry input
    /// folds onto them.
    Placed,
    /// It keeps its width, or has no audio on one side: output `c` is input
    /// `c`, or there is nothing to pass.
    Same,
    /// It changes width by index, for the reason given.
    ByIndex(&'static str),
}

/// One node type: its name as `impl Node for` writes it, its shapes at the
/// widths it is built for, and what its dry path is when a width changes.
struct Row {
    name: &'static str,
    shapes: Vec<Shape>,
    dry: Dry,
}

fn shape(node: impl Node) -> Shape {
    node.shape()
}

fn row(name: &'static str, shapes: Vec<Shape>, dry: Dry) -> Row {
    Row { name, shapes, dry }
}

/// Every node type in `src/` (the scan below holds the list to the source).
fn rows() -> Vec<Row> {
    use ChannelLayout as L;
    let s = Seconds::new;
    let transport = Transport::new(SampleRate::new(48_000.0));
    #[cfg_attr(not(feature = "convolution"), allow(unused_mut))]
    let mut rows = vec![
        row(
            "DesignNode",
            vec![
                shape(FreeverbNode::default()),
                shape(DesignNode::live(FdnDesign::default(), FdnControls::DEFAULT)),
                shape(SvfFilterNode::<f32>::with_channels(
                    L::STEREO,
                    SvfType::LowPass,
                    Hz(1_000.0),
                    Q(0.7),
                )),
            ],
            Dry::Placed,
        ),
        row(
            // A DesignNode too, but its outputs are bands, not speakers: it
            // is a split, and no dry signal belongs on a band.
            "CrossoverNode",
            vec![shape(
                CrossoverNode::new(L::STEREO, &[Hz(1_000.0)]).expect("one split"),
            )],
            Dry::ByIndex("its outputs are bands, not speakers"),
        ),
        row(
            "VbapPannerNode",
            vec![
                shape(VbapPannerNode::stereo()),
                shape(VbapPannerNode::quad()),
                shape(VbapPannerNode::surround_5_1()),
                shape(VbapPannerNode::surround_7_1()),
                shape(VbapPannerNode::atmos_7_1_4()),
            ],
            Dry::Placed,
        ),
        row(
            "GateNode",
            vec![shape(GateNode::stereo(
                Db(-20.0),
                s(0.001),
                s(0.01),
                s(0.05),
            ))],
            Dry::ByIndex("the audio, then its side chain: output c is audio input c"),
        ),
        row(
            "CompressorNode",
            vec![shape(CompressorNode::stereo(
                Db(-20.0),
                CompressionRatio::new(4.0),
                s(0.005),
                s(0.05),
            ))],
            Dry::ByIndex("the audio, then its side chain: output c is audio input c"),
        ),
        row(
            "MultibandCompressorNode",
            vec![shape(
                MultibandCompressorNode::new(L::STEREO, 3).expect("three bands"),
            )],
            Dry::Same,
        ),
        row(
            "DelayLineNode",
            vec![shape(DelayLineNode::new(
                s(0.5),
                s(0.1),
                Feedback::new(0.3),
            ))],
            Dry::Same,
        ),
        row(
            "DistortionNode",
            vec![shape(DistortionNode::new(ShapeKind::Tanh, Drive::new(2.0)))],
            Dry::Same,
        ),
        row(
            "PhaserNode",
            vec![shape(PhaserNode::with_channels(L::STEREO, 4))],
            Dry::Same,
        ),
        row(
            "ModDelayNode",
            vec![shape(ModDelayNode::new(
                L::STEREO,
                ModDelayConfig::GENERAL_MIDI_CHORUS,
            ))],
            Dry::Same,
        ),
        row(
            "EqBandNode",
            vec![shape(EqBandNode::<f32>::new(
                SvfType::Bell,
                Hz(1_000.0),
                Q(1.0),
                Db(3.0),
            ))],
            Dry::Same,
        ),
        row(
            "LimiterNode",
            vec![shape(LimiterNode::new(Db(-3.0), Db(-0.3)))],
            Dry::Same,
        ),
        row(
            "BrickwallLimiterNode",
            vec![shape(BrickwallLimiterNode::new(Db(-0.3)))],
            Dry::Same,
        ),
        row(
            "BusStripNode",
            vec![shape(BusStripNode::with_channels(L::STEREO))],
            Dry::Same,
        ),
        row(
            // Its shape is stereo to stereo whatever HRIR set it is built
            // from; building one needs a set, so it is not built here.
            "HrtfBinauralNode",
            vec![],
            Dry::Same,
        ),
        row(
            "ClickNode",
            vec![shape(ClickNode::new(
                &transport,
                Arc::new(ClickSettings::new()),
            ))],
            Dry::Same,
        ),
        row(
            "ModulatorNode",
            vec![shape(LfoNode::new(LfoShape::Sine))],
            Dry::Same,
        ),
        row(
            "AutomationLaneNode",
            vec![shape(AutomationLaneNode::new(Trajectory::new()))],
            Dry::Same,
        ),
        row(
            "ControlLaneNode",
            vec![shape(ControlLaneNode::for_foreign(
                0,
                ControlProgram::default(),
                None,
                (0.0, 1.0),
            ))],
            Dry::Same,
        ),
        row(
            "NoteEnvelopeNode",
            vec![shape(NoteEnvelopeNode::new(SourceId(1), Adsr::default()))],
            Dry::Same,
        ),
        row(
            "EnvelopeFollowerNode",
            vec![shape(EnvelopeFollowerNode::new(
                SourceId(1),
                s(0.01),
                s(0.1),
                L::STEREO,
            ))],
            Dry::Same,
        ),
    ];
    #[cfg(feature = "convolution")]
    rows.push(row(
        "ConvolverNode",
        vec![
            shape(tutti_nodes::ConvolverNode::new(&[1.0], 64)),
            shape(tutti_nodes::ConvolverNode::mono_to_stereo(
                &[1.0],
                &[0.5],
                64,
            )),
        ],
        Dry::Same,
    ));
    rows
}

/// Whether a side's main bus names speakers.
fn placed(buses: &tutti_graph::Buses) -> bool {
    buses.main_bus().is_some_and(|(_, bus)| {
        bus.topology
            .positions()
            .iter()
            .any(|s| *s != Speaker::Discrete)
    })
}

/// A node marked placed declares speakers on both sides whenever its width
/// changes, and a node marked `Same` never changes it.
///
/// Mutation (run): the reverbs' `Channels::speakers` → `Channels::discrete`
/// → "DesignNode changes width 1 -> 2 by index". VBAP's `with_buses_out`
/// removed → the same for `VbapPannerNode`. The gate's row marked `Same` →
/// "GateNode changes width 4 -> 2 but says it keeps it". The distortion's row
/// marked `ByIndex` → "DistortionNode keeps its width".
#[test]
fn a_node_that_changes_width_places_its_channels() {
    for row in rows() {
        let mut changes = false;
        for shape in &row.shapes {
            let (i, o) = (shape.audio_in.count(), shape.audio_out.count());
            if i == 0 || o == 0 || i == o {
                continue;
            }
            changes = true;
            match row.dry {
                Dry::Placed => assert!(
                    placed(&shape.buses_in) && placed(&shape.buses_out),
                    "{} changes width {i} -> {o} by index: its dry input lands on \
                     the first outputs alone. Declare its buses (`Placement::Speakers`, \
                     `with_buses_in`/`with_buses_out`)",
                    row.name
                ),
                Dry::Same => panic!("{} changes width {i} -> {o} but says it keeps it", row.name),
                Dry::ByIndex(_) => {}
            }
        }
        if let Dry::ByIndex(why) = row.dry {
            assert!(
                changes,
                "{} keeps its width, yet says \"{why}\": it is `Same`",
                row.name
            );
        }
    }
}

/// Every `impl … Node for X` in the crate's `src/` (but the test helpers) has
/// a row, so a new node type cannot skip the check above.
///
/// Mutation (run): the `ModDelayNode` row removed → "no row for ModDelayNode".
#[test]
fn every_node_type_has_a_row() {
    let names: Vec<&str> = rows().iter().map(|r| r.name).collect();
    let src = tutti_fixtures::repo_root().join("crates/tutti-nodes/src");
    let mut stack = vec![src];
    let mut seen = 0;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("a source directory") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                // Its node exists only with the feature.
                if cfg!(not(feature = "convolution")) && path.ends_with("convolution") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") || path.ends_with("testing.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("a source file");
            for line in text.lines().map(str::trim_start) {
                let Some(rest) = line.strip_prefix("impl") else {
                    continue;
                };
                let Some((_, ty)) = rest.split_once(" Node for ") else {
                    continue;
                };
                let ty: String = ty
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                seen += 1;
                assert!(
                    names.contains(&ty.as_str()),
                    "no row for {ty} ({}): add it to `rows()` with its dry path",
                    path.display()
                );
            }
        }
    }
    assert!(seen >= 20, "the scan found {seen} node types");
}
