//! A route's range is a range of the target's unit, and a backwards one is
//! refused where it is declared (doc 030, T2).

use tutti_graph::NodeKey;
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_session::{ParamRef, RangeError, RouteError, Session};
use tutti_types::{Depth, Hz, MaxBlock, Mix, Q, SampleRate, Samples};

fn session() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(48_000.0),
            tutti_types::ChannelLayout::STEREO,
            MaxBlock::of(Samples(256)),
        )
        .expect("builds")
}

/// **A range with its start above its end is refused with both bounds, and
/// nothing is declared**: the route that needs the range is still refused as
/// undeclared. A NaN bound is refused as one.
#[test]
fn a_backwards_or_nan_range_is_refused_and_declares_nothing() {
    let mut s = session();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::new(
        SvfType::LowPass,
        Hz(1_000.0),
        Q(0.7),
    ));
    let mut m = s.mods();
    assert_eq!(
        m.range(lp.cutoff(), Hz(4_000.0)..Hz(100.0)),
        Err(RangeError::MinAboveMax {
            min: 4_000.0,
            max: 100.0
        })
    );
    assert!(matches!(
        m.range(lp.cutoff(), Hz(f32::NAN)..Hz(100.0)),
        Err(RangeError::NotANumber { .. })
    ));
    assert!(matches!(
        m.range_raw(lp.cutoff(), 1.0, f32::NAN),
        Err(RangeError::NotANumber { .. })
    ));
    let src = m.macro_(0.0);
    assert!(matches!(
        m.route(src, lp.cutoff(), Depth(0.5)),
        Err(RouteError::UndeclaredRange { .. })
    ));

    // A range of one point is a range.
    m.range(lp.cutoff(), Hz(500.0)..Hz(500.0)).unwrap();
    assert!(m.route(src, lp.cutoff(), Depth(0.5)).is_ok());
}

/// **A typed range, an untyped one and the typed reference's `ParamRef` are
/// one declaration**: a range set through the typed form is the target of a
/// route named by the untyped one, and an edge's and a mix's ranges are in
/// their own units.
#[test]
fn a_typed_range_declares_the_target_an_untyped_ref_names() {
    let mut s = session();
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::new(
        SvfType::LowPass,
        Hz(1_000.0),
        Q(0.7),
    ));
    let mut m = s.mods();
    m.range(lp.cutoff(), Hz(100.0)..Hz(4_000.0)).unwrap();
    let src = m.macro_(0.0);
    let same = ParamRef::unit(lp.erase(), tutti_types::UnitParam::Cutoff);
    assert_eq!(ParamRef::from(lp.cutoff()), same);
    assert!(
        m.route(src, same, Depth(0.5)).is_ok(),
        "the range is declared"
    );

    // A mix's range is in `Mix`: backwards is refused there too.
    let mix = tutti_session::MixRef::of(NodeKey(1));
    assert!(m.range(mix, Mix(0.0)..Mix(1.0)).is_ok());
    assert!(matches!(
        m.range(mix, Mix(1.0)..Mix(0.0)),
        Err(RangeError::MinAboveMax { .. })
    ));
}
