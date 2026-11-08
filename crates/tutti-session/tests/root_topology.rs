//! The graph root's declared topology: what the session's output is, as the
//! engine places it on the device and an export places it in a file.
//!
//! Every figure is analytic: a `Const` writes a known value per channel, and
//! where a fold is involved the expected frame is written out from the ITU
//! rules (`M3DB`), not read from another render.

use tutti_nodes::testing::Const;
use tutti_session::{Error, GraphSource, Session, SessionBuilder};
use tutti_types::{
    ChannelLayout, ChannelTopology, FoldError, InterleavedMut, M3DB, SampleRate, Samples, Speaker,
};

const RATE: SampleRate = SampleRate(48_000.0);

/// The channels `values` hold, one per global output from channel 0.
fn feed(session: &mut Session, values: &[f32]) {
    let (node, _) = session.graph().insert(Const::frame(values));
    for c in 0..values.len() {
        session
            .graph()
            .set_output_source(c, GraphSource::Node(node, c));
    }
}

/// Frame 0 of the second device block (the first installs the plan), after the
/// session commits. `device` channels wide.
fn heard(session: &mut Session, device: u16) -> Vec<f32> {
    session.update();
    let layout = ChannelLayout::from(device);
    let mut out = vec![-9.0f32; usize::from(device) * 64];
    for _ in 0..2 {
        session.render(&mut InterleavedMut::new(&mut out, layout));
    }
    out[..usize::from(device)].to_vec()
}

fn offline(builder: SessionBuilder, device: u16) -> Result<Session, Error> {
    builder.build_offline(RATE, ChannelLayout::from(device), Samples(64))
}

fn buses(n: u16) -> ChannelTopology {
    ChannelTopology::discrete(ChannelLayout::from(n))
}

/// Four declared buses on a six-wide device stay at 0..4. The root is the
/// declaration's width, not the device's.
///
/// Mutation (run): `SessionBuilder::declare` not called → the engine reads the
/// root as quad and the frame is `[.1, .2, 0, 0, .3, .4]`.
#[test]
fn a_declared_discrete_root_keeps_its_buses_by_index() {
    let mut s = offline(Session::builder().root_topology(buses(4)), 6).unwrap();
    assert_eq!(s.graph_ref().outputs(), 4);
    assert_eq!(s.root_topology(), Some(&buses(4)));
    feed(&mut s, &[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(heard(&mut s, 6), [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);
}

/// A declared 5.1 root folds into stereo by the 5.1 rules, from the speakers it
/// declared: the same six channels with the front pair declared the other way
/// round fold with left and right exchanged. (The plain 5.1 is the figure a bare
/// six gives, so only the swapped declaration shows the engine read it.)
///
/// Mutation (run): `Graph::declare_root` not publishing the declaration to the
/// engine → the swapped frame is the plain 5.1 figure and fails.
#[test]
fn a_declared_surround_root_folds_by_the_surround_rules() {
    let five_one = ChannelTopology::smpte(ChannelLayout::from(6u16)).unwrap();
    let mut s = offline(Session::builder().root_topology(five_one), 2).unwrap();
    assert_eq!(s.graph_ref().outputs(), 6);
    feed(&mut s, &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    assert_eq!(
        heard(&mut s, 2),
        [1.0 + M3DB * 3.0 + M3DB * 5.0, 2.0 + M3DB * 3.0 + M3DB * 6.0]
    );
    let swapped = ChannelTopology::new([
        Speaker::FrontRight,
        Speaker::FrontLeft,
        Speaker::FrontCenter,
        Speaker::LowFrequency,
        Speaker::SideLeft,
        Speaker::SideRight,
    ]);
    s.declare_root_topology(Some(swapped)).unwrap();
    assert_eq!(
        heard(&mut s, 2),
        [2.0 + M3DB * 3.0 + M3DB * 5.0, 1.0 + M3DB * 3.0 + M3DB * 6.0]
    );
}

/// With nothing declared, the root is the device's floor and a bare four is
/// quad: as before this existed.
///
/// Mutation (run): `declare_root_topology(None)` keeping the declaration is
/// caught by the width test below; here the engine reading `Fold::declared`
/// for every root would put the quad's rears at 2 and 3 into stereo, as
/// `[.1 + .2, ..]` is not the figure asserted. (Not run separately: the
/// undeclared path is the one the whole existing suite covers.)
#[test]
fn an_undeclared_root_is_as_it_was() {
    let mut s = offline(Session::builder().outputs_floor(4), 6).unwrap();
    assert_eq!(s.root_topology(), None);
    assert_eq!(s.graph_ref().outputs(), 6, "the device floors the root");
    feed(&mut s, &[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(heard(&mut s, 6), [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);

    // A project of four on a stereo device is quad, folded at -3 dB.
    let mut quad = offline(Session::builder().outputs_floor(4), 2).unwrap();
    assert_eq!(quad.graph_ref().outputs(), 4);
    feed(&mut quad, &[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(heard(&mut quad, 2), [0.1 + M3DB * 0.3, 0.2 + M3DB * 0.4]);
}

/// A declaration the device cannot place is refused, at build and after, and
/// leaves the session as it was.
///
/// Mutation (run): `Fold::declared` reading a discrete source as `to_width`
/// does (quad) → the refusals succeed and the cases here fail.
/// (`check_placement` skipping a declared root alone survives: the build's
/// `declare` refuses the same pair a step later, so the refusal is doubly held.)
#[test]
fn an_unplaceable_declaration_is_refused() {
    let refused = |r: Result<Session, Error>| match r {
        Err(Error::Output(e)) => e,
        other => panic!("expected Output, got {:?}", other.map(|_| ())),
    };
    assert_eq!(
        refused(offline(Session::builder().root_topology(buses(4)), 2)),
        FoldError::Unplaced {
            channel: 2,
            speaker: Speaker::Discrete
        }
    );
    let unnamed = ChannelTopology::new([Speaker::FrontLeft, Speaker::Unknown(9)]);
    assert!(matches!(
        refused(offline(Session::builder().root_topology(unnamed), 2)),
        FoldError::Unnamed { .. }
    ));
    // Wider than the engine renders.
    let wide = buses(tutti_graph::MAX_ROOT_CHANNELS as u16 + 1);
    assert!(matches!(
        refused(offline(Session::builder().root_topology(wide), 32)),
        FoldError::Unplaced { .. }
    ));
    assert!(matches!(
        Session::builder()
            .root_topology(buses(40))
            .build_detached(RATE),
        Err(Error::Output(_))
    ));

    // After the build.
    let mut s = offline(Session::builder(), 2).unwrap();
    assert!(matches!(
        s.declare_root_topology(Some(buses(4))),
        Err(Error::Output(FoldError::Unplaced { channel: 2, .. }))
    ));
    assert_eq!(s.graph_ref().outputs(), 2);
    assert_eq!(s.root_topology(), None);
}

/// Declared after the build, on a device-wide root: the declaration sets the
/// root's width (narrowing six to four), the root cannot then be widened under
/// it, and taking it back leaves the root at four, quad again.
///
/// Mutation (run): `declare_root` not resizing the outputs → `outputs()` stays
/// six; `widen_outputs` not checking the declaration → the widening to five is
/// accepted; `declare_root(None)` keeping the declaration → the last frame
/// stays by index.
#[test]
fn a_declaration_sets_the_roots_width_and_the_width_cannot_leave_it() {
    let mut s = offline(Session::builder(), 6).unwrap();
    assert_eq!(s.graph_ref().outputs(), 6);
    s.declare_root_topology(Some(buses(4))).unwrap();
    assert_eq!(s.graph_ref().outputs(), 4);
    feed(&mut s, &[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(heard(&mut s, 6), [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);

    // As wide or narrower is a no-op; wider is refused and the declaration stands.
    s.graph().widen_outputs(4).unwrap();
    let err = s.graph().widen_outputs(5).unwrap_err();
    assert_eq!(
        err,
        FoldError::Discrete {
            src: ChannelLayout::QUAD,
            dst: ChannelLayout::from(5u16)
        }
    );
    assert_eq!(s.graph_ref().outputs(), 4);

    s.declare_root_topology(None).unwrap();
    assert_eq!(s.root_topology(), None);
    assert_eq!(s.graph_ref().outputs(), 4);
    assert_eq!(heard(&mut s, 6), [0.1, 0.2, 0.0, 0.0, 0.3, 0.4]);
    s.graph().widen_outputs(5).unwrap();
    assert_eq!(s.graph_ref().outputs(), 5);
}

/// An export of the master places the file from the declaration, as the device
/// is: four buses into a six-channel file stay at 0..4.
///
/// Mutation (run): `PendingExport::build` not setting the root topology → the
/// file is `[.1, .2, 0, 0, .3, .4]`.
#[cfg(feature = "export")]
#[test]
fn an_export_is_placed_from_the_declaration() {
    use tutti_export::{EncodeConfig, ExportConfig, Format, Outcome, Pcm, RenderConfig};
    use tutti_session::{ExportClock, ExportOutput, ExportRequest, ExportSource, ExportTarget};
    let planes = |s: &mut Session| {
        let config = ExportConfig {
            render: RenderConfig {
                sample_rate: RATE,
                duration_seconds: 0.01,
                ..Default::default()
            },
            encode: EncodeConfig {
                format: Format::Wav(Pcm::Float32),
                channels: ChannelLayout::from(6u16),
                topology: None,
            },
            ..Default::default()
        };
        let request = ExportRequest::new(
            ExportSource::Mix,
            ExportTarget::Buffers,
            config,
            ExportClock::frozen(),
        );
        match s.export_blocking(request).unwrap() {
            Outcome::Finished(ExportOutput::Buffers(r)) => {
                r.planes.iter().map(|p| p[0]).collect::<Vec<_>>()
            }
            _ => panic!("expected buffers"),
        }
    };
    let mut declared = Session::builder()
        .root_topology(buses(4))
        .build_detached(RATE)
        .unwrap();
    feed(&mut declared, &[0.1, 0.2, 0.3, 0.4]);
    assert_eq!(planes(&mut declared), [0.1, 0.2, 0.3, 0.4, 0.0, 0.0]);
    declared.declare_root_topology(None).unwrap();
    assert_eq!(planes(&mut declared), [0.1, 0.2, 0.0, 0.0, 0.3, 0.4]);
}
