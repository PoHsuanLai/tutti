//! Declicked wiring (doc 030 section 4): the session graph's `connect` fades a
//! new edge in over `EDGE_DECLICK` frames, and `disconnect`, `disconnect_all`,
//! `set_sources` removals and the edges of a removed node fade out over the same
//! span, leaving the plan when the fade has ended. `Connection::hard` and
//! `disconnect_hard` opt out.
//!
//! Every figure is analytic: a constant of 1.0 through an edge whose weight is
//! `j / 256` at fade frame `j`, one frame per render call, so the position of
//! every sample is exact.

use tutti_graph::{EDGE_DECLICK, ForkByClone};
use tutti_nodes::testing::Const;
use tutti_session::{OutPort, Root, Session};
use tutti_types::SampleRate;

const SPAN: usize = 256;

fn session() -> Session {
    Session::builder()
        .outputs_floor(2)
        .build_detached(SampleRate(48_000.0))
        .expect("builds")
}

/// `n` frames of output channel 0, one render call each.
fn frames(s: &mut Session, n: usize) -> Vec<f32> {
    (0..n)
        .map(|_| {
            let mut frame = [f32::NAN; 2];
            s.render_frame(&mut frame);
            frame[0]
        })
        .collect()
}

/// **A connect's first block ramps from exactly 0, and the edge is at its weight
/// at `EDGE_DECLICK`.** The weight is `j / 256` at fade frame `j`; the update
/// that follows the fade drops the entry, and the edge is then the plain sum.
#[test]
fn a_connect_fades_in_from_exactly_zero_and_reaches_unity_at_edge_declick() {
    assert_eq!(EDGE_DECLICK.get(), SPAN);
    let mut s = session();
    let (src, _) = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph().connect(src.out(0), Root::output(0)).unwrap();
    let _ = s.update();

    let got = frames(&mut s, SPAN + 64);
    assert_eq!(got[0].to_bits(), 0.0f32.to_bits(), "from exactly +0.0");
    for (j, &g) in got.iter().enumerate().take(SPAN) {
        let want = j as f32 / SPAN as f32;
        assert_eq!(g.to_bits(), want.to_bits(), "frame {j}");
    }
    assert!(
        got[SPAN..].iter().all(|&x| x == 1.0),
        "unity from the 256th"
    );

    // The ended fade is reported: the next update commits the plain edge.
    let sent = s.graph_ref().commits_sent();
    let _ = s.update();
    assert_eq!(
        s.graph_ref().commits_sent(),
        sent + 1,
        "the fade was settled"
    );
    assert!(frames(&mut s, 8).iter().all(|&x| x == 1.0));
    let _ = s.update();
    assert_eq!(s.graph_ref().commits_sent(), sent + 1, "and only once");
}

/// **`.hard()` is the old step**: full weight from the first frame, no entry,
/// no commit to settle.
#[test]
fn hard_connects_at_full_weight_at_once() {
    let mut s = session();
    let (src, _) = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .connect(src.out(0), Root::output(0))
        .unwrap()
        .hard();
    let _ = s.update();
    assert!(
        frames(&mut s, 8)
            .iter()
            .all(|&x| x.to_bits() == 1.0f32.to_bits())
    );
    let sent = s.graph_ref().commits_sent();
    let _ = s.update();
    assert_eq!(s.graph_ref().commits_sent(), sent, "nothing to settle");
}

/// **A disconnect fades out: its last nonzero frame is inside the span**, the
/// edge is no source of its sink from the call on, and it leaves the plan after.
#[test]
fn a_disconnect_fades_out_inside_the_span_and_leaves_the_plan() {
    let mut s = session();
    let (src, _) = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .connect(src.out(0), Root::output(0))
        .unwrap()
        .hard();
    let _ = s.update();
    assert!(frames(&mut s, 4).iter().all(|&x| x == 1.0));

    assert!(s.graph().disconnect(src.out(0), Root::output(0)));
    assert!(s.graph_ref().sources(Root::output(0)).is_empty());
    assert!(
        !s.graph().disconnect(src.out(0), Root::output(0)),
        "already on its way out"
    );
    let _ = s.update();
    let got = frames(&mut s, SPAN + 64);
    for (j, &g) in got.iter().enumerate().take(SPAN) {
        let want = 1.0 - j as f32 / SPAN as f32;
        assert_eq!(g.to_bits(), want.to_bits(), "frame {j}");
    }
    let last = got.iter().rposition(|&x| x != 0.0).expect("it was heard");
    assert!(
        last < SPAN,
        "the last nonzero frame {last} is inside the span"
    );

    let sent = s.graph_ref().commits_sent();
    let _ = s.update();
    assert_eq!(
        s.graph_ref().commits_sent(),
        sent + 1,
        "the edge left the plan"
    );
    assert!(frames(&mut s, 4).iter().all(|&x| x == 0.0));
}

/// **`disconnect_hard` cuts at once**, and a connect that no plan has heard
/// leaves no fade behind.
#[test]
fn disconnect_hard_cuts_and_an_unheard_edge_just_goes() {
    let mut s = session();
    let (src, _) = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .connect(src.out(0), Root::output(0))
        .unwrap()
        .hard();
    let _ = s.update();
    assert!(frames(&mut s, 4).iter().all(|&x| x == 1.0));
    assert!(s.graph().disconnect_hard(src.out(0), Root::output(0)));
    let _ = s.update();
    assert!(frames(&mut s, 4).iter().all(|&x| x == 0.0), "no tail");

    // Connected and disconnected inside one edit: nothing was ever heard.
    s.graph().connect(src.out(0), Root::output(0)).unwrap();
    assert!(s.graph().disconnect(src.out(0), Root::output(0)));
    let _ = s.update();
    assert!(frames(&mut s, SPAN).iter().all(|&x| x == 0.0));
}

/// **A reconnect while the edge fades out turns it round** from the weight it
/// had, without a step.
#[test]
fn a_reconnect_during_a_fade_out_rises_from_where_it_was() {
    let mut s = session();
    let (src, _) = s.graph().add(ForkByClone(Const::mono(1.0)));
    s.graph()
        .connect(src.out(0), Root::output(0))
        .unwrap()
        .hard();
    let _ = s.update();
    frames(&mut s, 2);
    s.graph().disconnect(src.out(0), Root::output(0));
    let _ = s.update();
    let down = frames(&mut s, 128);
    s.graph().connect(src.out(0), Root::output(0)).unwrap();
    let _ = s.update();
    let up = frames(&mut s, 8);
    assert_eq!(up[0], *down.last().unwrap(), "no step at the turn");
    assert!(up[7] > up[0], "rising");
    assert_eq!(s.graph_ref().sources(Root::output(0)), [src.out(0)]);
}

/// **`set_sources` fades what it removes and what it adds**, and a removed
/// node's output fades out before the node goes.
#[test]
fn set_sources_and_remove_fade_what_leaves() {
    let mut s = session();
    let (a, _) = s.graph().add(ForkByClone(Const::mono(1.0)));
    let (b, _) = s.graph().add(ForkByClone(Const::mono(2.0)));
    let sink = Root::output(0);
    s.graph()
        .set_sources_hard(sink, [OutPort::new(a.erase(), 0)])
        .unwrap();
    let _ = s.update();
    frames(&mut s, 2);
    s.graph()
        .set_sources(sink, [OutPort::new(b.erase(), 0)])
        .unwrap();
    let _ = s.update();
    let got = frames(&mut s, SPAN);
    // a falls as 1 - j/256 while b rises as 2 j/256: their sum at frame j.
    for (j, &g) in got.iter().enumerate() {
        let (f, r) = (j as f32 / SPAN as f32, j as f32 / SPAN as f32);
        let want = (0.0 + (1.0 - f) * 1.0) + r * 2.0;
        assert_eq!(g.to_bits(), want.to_bits(), "frame {j}");
    }
    assert!(s.remove(b));
    assert!(!s.graph_ref().contains(b), "gone to its owner at once");
    let _ = s.update();
    let tail = frames(&mut s, SPAN + 64);
    assert!(tail[0] > 1.9, "still heard while it fades: {}", tail[0]);
    assert!(tail[SPAN..].iter().all(|&x| x == 0.0));
    let _ = s.update();
    let _ = s.update();
    assert_eq!(s.graph_ref().node_count(), 1, "only a is left");
}
