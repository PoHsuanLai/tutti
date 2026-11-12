//! A route can be found again, and edited through a view of it (doc 030, T4):
//! the keys are minted, `routes()` / `routes_to()` / `sources()` read what is
//! declared, and polarity and curve are part of declaring a route.

use tutti_modulation::{Law, Lfo, Polarity, Source};
use tutti_nodes::{SvfFilterNode, SvfType};
use tutti_session::{ModTarget, NoSuchModRoute, NoSuchModSource, ParamRef, RouteSpec, Session};
use tutti_types::{Depth, Hz, MaxBlock, Q, SampleRate, Samples};

fn session() -> Session {
    Session::builder()
        .build_offline(
            SampleRate(48_000.0),
            tutti_types::ChannelLayout::STEREO,
            MaxBlock::of(Samples(256)),
        )
        .expect("builds")
}

fn filter(s: &mut Session) -> ParamRef {
    let (lp, _) = s.graph().add(SvfFilterNode::<f32>::new(
        SvfType::LowPass,
        Hz(1_000.0),
        Q(0.7),
    ));
    ParamRef::from(lp.cutoff())
}

/// **`routes_to` finds exactly the routes into a target, with what each was
/// declared with**, `routes` all of them in declaration order, and `sources`
/// the sources with what feeds them. A removed source takes its routes out of
/// every listing.
#[test]
fn routes_and_sources_are_found_again() {
    let mut s = session();
    let (a, b) = (filter(&mut s), filter(&mut s));
    let mut m = s.mods_mut();
    m.range_raw(a, 0.0, 1.0).unwrap();
    m.range_raw(b, 0.0, 1.0).unwrap();
    let lfo = m.lfo(Lfo::sine());
    let knob = m.macro_(0.5);
    let r1 = m
        .route(RouteSpec::new(lfo, a, Depth::of(0.25)).curve(Law::Smootherstep))
        .unwrap();
    let r2 = m
        .route(RouteSpec::new(knob, a, Depth::of(-0.5)).polarity(Polarity::Unipolar))
        .unwrap();
    let r3 = m.route(RouteSpec::new(knob, b, Depth::FULL)).unwrap();

    let view = s.mods();
    let into_a: Vec<_> = view.routes_to(a).collect();
    assert_eq!(
        into_a.iter().map(|r| r.id).collect::<Vec<_>>(),
        [r1, r2],
        "the routes into a, in order"
    );
    assert_eq!(
        (
            into_a[0].source,
            into_a[0].depth,
            into_a[0].polarity,
            into_a[0].curve
        ),
        (lfo, Depth::of(0.25), Polarity::Bipolar, Law::Smootherstep)
    );
    assert_eq!(
        (
            into_a[1].source,
            into_a[1].depth,
            into_a[1].polarity,
            into_a[1].curve
        ),
        (knob, Depth::of(-0.5), Polarity::Unipolar, Law::Linear)
    );
    assert_eq!(into_a[0].target, ModTarget::from(a));
    assert_eq!(
        view.routes_to(b).map(|r| r.id).collect::<Vec<_>>(),
        [r3],
        "and into b"
    );
    assert_eq!(view.routes().count(), 3);
    assert_eq!(view.route(r2), Some(into_a[1]));
    assert_eq!(
        view.sources()
            .map(|i| (i.id, i.source, i.fed_by))
            .collect::<Vec<_>>(),
        [
            (lfo, Source::from(Lfo::sine()), None),
            (knob, Source::Macro { value: 0.5 }, None),
        ]
    );

    s.mods_mut().remove_source(knob).unwrap();
    let view = s.mods();
    assert_eq!(view.routes().map(|r| r.id).collect::<Vec<_>>(), [r1]);
    assert_eq!(view.routes_to(b).count(), 0, "the source's routes went");
    assert_eq!(view.source(knob), None);
    assert_eq!(
        s.mods_mut().remove_route(r3),
        Err(NoSuchModRoute(r3)),
        "a route that went with its source is not there"
    );
    assert_eq!(
        s.mods_mut().set_source(knob, Lfo::sine()),
        Err(NoSuchModSource(knob))
    );
}

/// **A route is edited through a view of it**, by named setters, and a route
/// that is not there is an error, not a silent no-op. What the view changes is
/// what `routes()` then reads.
#[test]
fn a_route_is_edited_through_its_view() {
    let mut s = session();
    let a = filter(&mut s);
    let mut m = s.mods_mut();
    m.range_raw(a, 0.0, 1.0).unwrap();
    let lfo = m.lfo(Lfo::sine());
    let route = m.route(RouteSpec::new(lfo, a, Depth::of(0.5))).unwrap();

    s.mods_mut()
        .route_of(route)
        .unwrap()
        .set_depth(Depth::of(-0.75))
        .set_polarity(Polarity::Unipolar)
        .set_curve(Law::Hold);
    let read = s.mods().route(route).unwrap();
    assert_eq!(
        (read.depth, read.polarity, read.curve),
        (Depth::of(-0.75), Polarity::Unipolar, Law::Hold)
    );

    s.mods_mut().remove_route(route).unwrap();
    assert!(matches!(
        s.mods_mut().route_of(route),
        Err(NoSuchModRoute(id)) if id == route
    ));
}

/// **Reading the declarations takes a shared borrow of the session**: a
/// reader of `mods()` does not mark the declarations edited. (A compile
/// property: `mods()` taking `&mut self` fails this function.)
#[test]
fn reading_the_declarations_needs_only_a_shared_session() {
    fn routes_in(s: &Session) -> usize {
        s.mods().routes().count()
    }
    assert_eq!(routes_in(&session()), 0);
}

/// **A take on a target with no automation is refused, naming the target**,
/// and opens nothing: the take is dropped and `take_mut` finds none. With
/// automation declared the same call opens it.
#[test]
fn recording_where_nothing_is_automated_is_refused() {
    use std::sync::Arc;
    use tutti_nodes::automation::{RecordMode, Trajectory};
    use tutti_session::{AutomationTake, NoAutomation};

    let mut s = session();
    let a = filter(&mut s);
    let mut m = s.mods_mut();
    let take = || AutomationTake::record(RecordMode::Touch, 0.0, 1.0);
    assert_eq!(
        m.record(a, take()),
        Err(NoAutomation {
            target: ModTarget::from(a)
        })
    );
    assert!(m.take_mut(a).is_none());

    m.automate(a, Arc::new(Trajectory::default()));
    m.record(a, take()).unwrap();
    assert!(m.take_mut(a).is_some());
}
