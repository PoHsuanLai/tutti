//! A tap opened and closed on a running detached session.
//!
//! The session half of `bevy-tutti`'s `graph_reconcile` `graph_tap` module.
//! `engine.rs` covers a tap fed by a render
//! (`a_detached_render_feeds_a_tap`, `an_offline_render_feeds_an_output_tap`)
//! and one closed by its reader's drop
//! (`a_dropped_reader_leaves_the_plan_at_the_reclaim_phase`); what it does not
//! is a tap opened after the first render and one closed by name
//! (`Graph::close_tap`).
//!
//! Mutations (run): `Graph::close_tap` doing nothing
//! (`closing_the_tap_stops_the_copy`); `Graph::open_tap` not marking the graph
//! edited, with nothing else edited after the first render (the reader stays
//! empty: `opening_a_tap_yields_a_reader_that_sees_the_rendered_frames`).

use tutti_nodes::testing::Const;
use tutti_session::{GraphSource, Session};
use tutti_types::{SampleRate, Samples};

/// A detached one-output session playing a constant 0.25.
fn session() -> Session {
    let mut s = Session::builder()
        .outputs_floor(1)
        .build_detached(SampleRate(48_000.0))
        .expect("builds");
    let (dc, _) = s.graph().insert(Const::mono(0.25));
    s.graph().set_output_source(0, GraphSource::Node(dc, 0));
    s
}

/// The whole point of the seam: a host gets the reader from the graph, and it
/// sees what the session rendered since the open.
#[test]
fn opening_a_tap_yields_a_reader_that_sees_the_rendered_frames() {
    let mut s = session();
    let mut out = [0.0f32];
    s.render_frame(&mut out);
    let mut reader = s
        .graph()
        .open_output_tap(Samples(64))
        .expect("the sources exist");
    for _ in 0..3 {
        s.render_frame(&mut out);
    }

    let mut got = [f32::NAN; 8];
    assert_eq!(
        reader.pop_into(&mut got),
        Samples(3),
        "three frames rendered since the open"
    );
    assert_eq!(got[..3], [0.25; 3]);
}

/// Closing stops the copy, so a host that finishes analysing is back to a plan
/// with no tap op.
#[test]
fn closing_the_tap_stops_the_copy() {
    let mut s = session();
    let mut reader = s
        .graph()
        .open_output_tap(Samples(64))
        .expect("the sources exist");
    let mut out = [0.0f32];
    s.render_frame(&mut out);
    s.graph().close_tap(reader.id());
    s.render_frame(&mut out);
    s.render_frame(&mut out);

    let mut got = [f32::NAN; 8];
    assert_eq!(
        reader.pop_into(&mut got),
        Samples(1),
        "only the frame rendered while open"
    );
}

/// A session that never opened a tap renders as it always did, and the reader
/// a later open returns starts empty: a tap flows from its commit.
#[test]
fn a_session_without_a_tap_renders_untouched() {
    let mut s = session();
    let mut out = [0.0f32];
    s.render_frame(&mut out);
    assert_eq!(out[0], 0.25);
    let reader = s
        .graph()
        .open_output_tap(Samples(64))
        .expect("the sources exist");
    assert_eq!(
        reader.available(),
        Samples(0),
        "a tap flows from its commit"
    );
}
