//! A block the engine silences is reported by the session as one
//! `SessionEvent::BlocksSilenced` per update that saw any, with the count;
//! a block merely over its budget is a polled reading and no event.
//!
//! The figures are counts of blocks rendered, so they stand on no oracle but
//! the render calls the test makes.

use std::time::Duration;

use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_session::{GraphSource, Session, SessionEvent};
use tutti_types::{ChannelLayout, InterleavedMut, SampleRate, Samples};

const RATE: f64 = 48_000.0;

fn offline() -> Session {
    Session::builder()
        .build_offline(SampleRate(RATE), ChannelLayout::STEREO, Samples(1_024))
        .expect("builds")
}

/// A stereo block, rendered: the width the offline engine supports.
fn good_block(s: &Session) {
    let mut buf = vec![0.0f32; 64 * 2];
    s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
}

/// A block of a width the engine has no folds for (it builds them up to 16
/// device channels): silenced.
fn refused_block(s: &Session) {
    let mut buf = vec![1.0f32; 64 * 40];
    s.render(&mut InterleavedMut::new(
        &mut buf,
        ChannelLayout::from(40u16),
    ));
    assert!(buf.iter().all(|&x| x == 0.0), "a silenced block is silent");
}

/// The counts of every `BlocksSilenced` among `events`.
fn silenced(events: &[SessionEvent]) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            SessionEvent::BlocksSilenced(n) => Some(*n),
            _ => None,
        })
        .collect()
}

/// A node that takes far longer than a 64-frame block's budget (1.3 ms).
#[derive(Clone)]
struct Slow;

impl Node for Slow {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        std::thread::sleep(Duration::from_millis(10));
        io.output(0).fill(0.0);
        Status::Modified
    }
    fn reset(&mut self) {}
}

#[test]
fn one_event_per_update_with_the_blocks_silenced_since_the_last() {
    let mut s = offline();
    assert!(silenced(&s.update()).is_empty(), "nothing rendered");
    good_block(&s);
    assert!(silenced(&s.update()).is_empty(), "a good block is no event");

    for _ in 0..3 {
        refused_block(&s);
    }
    good_block(&s);
    assert_eq!(silenced(&s.update()), [3]);
    assert!(silenced(&s.update()).is_empty(), "reported once");

    for _ in 0..2 {
        refused_block(&s);
    }
    assert_eq!(silenced(&s.update()), [2]);
    assert_eq!(s.load_meter().expect("engine").peek().silenced, 5);
}

/// A meter reset between updates restarts the count: no event for the
/// reset, and the next burst is counted from zero.
#[test]
fn a_meter_reset_is_no_event_and_restarts_the_count() {
    let mut s = offline();
    for _ in 0..4 {
        refused_block(&s);
    }
    assert_eq!(silenced(&s.update()), [4]);
    s.load_meter().expect("engine").reset();
    assert!(silenced(&s.update()).is_empty());
    refused_block(&s);
    refused_block(&s);
    assert_eq!(silenced(&s.update()), [2]);
}

/// Blocks over their budget and none silenced: the reading rises, the
/// session says nothing.
#[test]
fn over_budget_alone_is_no_event() {
    let mut s = offline();
    let (slow, _) = s.graph().insert(ForkByClone(Slow));
    s.graph().set_output_source(0, GraphSource::Node(slow, 0));
    s.graph().set_output_source(1, GraphSource::Node(slow, 0));
    let events = s.update();
    assert!(silenced(&events).is_empty());
    good_block(&s);
    good_block(&s);
    let r = s.load_meter().expect("engine").peek();
    assert!(r.over_budget >= 1, "the node outran its budget: {r:?}");
    assert_eq!(r.silenced, 0);
    assert!(silenced(&s.update()).is_empty());
}
