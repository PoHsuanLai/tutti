//! Tail composition across statically-built fundsp graphs.
//!
//! `AudioNode::tail` is a separate channel from the `Signal::Latency` one that
//! carries latency, and these pin the two reasons why.

use fundsp_tutti::prelude32::*;
use tutti_types::{Samples, Tail};

/// A delay reports its length as tail while reporting **zero** latency.
///
/// This is the case that makes tail a channel of its own. `Delay::route` passes
/// `filter(0.0, ..)` deliberately — a delay is intended delay, and PDC
/// compensating it away would delete the echo — so the number tail needs most is
/// exactly the one the latency channel is built to hide.
#[test]
fn a_delay_reports_its_length_as_tail_but_no_latency() {
    let mut d = delay(0.1);
    assert_eq!(AudioUnit::tail(&mut d), Tail::Finite(Samples(4410)));
    assert_eq!(
        d.latency(),
        Some(0.0),
        "a delay must not be compensated away"
    );
}

/// A cascade sums its parts, with no one walking the graph.
#[test]
fn a_pipe_sums_its_children() {
    let mut chain = delay(0.1) >> delay(0.2);
    assert_eq!(AudioUnit::tail(&mut chain), Tail::Finite(Samples(13230)));
}

/// A merge takes the longer leg, where latency would take the shorter.
///
/// The rules genuinely differ: latency asks when a signal first arrives and
/// takes the minimum; tail asks when it last leaves and takes the maximum.
#[test]
fn a_stack_takes_the_longer_child_not_the_shorter() {
    let mut par = delay(0.1) | delay(0.2);
    assert_eq!(AudioUnit::tail(&mut par), Tail::Finite(Samples(8820)));
}

/// A stateless node declares it has no tail rather than staying silent, so it
/// does not poison a composition it takes no part in.
#[test]
fn a_pass_through_reports_no_tail() {
    let mut p = pass();
    assert_eq!(AudioUnit::tail(&mut p), Tail::None);

    // And it composes away rather than making the chain unknown.
    let mut chain = pass() >> delay(0.1);
    assert_eq!(AudioUnit::tail(&mut chain), Tail::Finite(Samples(4410)));
}

/// A feedback reverb answers `Unbounded`, not a guess.
///
/// An FDN re-enters its own output, so whether it ever falls silent depends on
/// the loop gain inside the contained node. Its `time` parameter is the RT60 and
/// belongs to whoever assembled the loop. A caller resolves this against a
/// chosen bound; inventing a number here would truncate a reverb nobody asked to
/// truncate.
#[test]
fn a_feedback_reverb_is_unbounded_rather_than_guessed() {
    let mut rev = reverb_stereo(10.0, 2.0, 0.5);
    assert_eq!(AudioUnit::tail(&mut rev), Tail::Unbounded);
}

/// A lookahead limiter's tail is its lookahead ring, exactly.
#[test]
fn a_limiter_reports_its_lookahead() {
    let mut lim = limiter(0.01, 0.01);
    assert_eq!(AudioUnit::tail(&mut lim), Tail::Finite(Samples(440)));
}
