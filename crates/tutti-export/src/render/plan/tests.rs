use super::*;

use tutti_types::SampleRate;

/// The plan of a job of one second at 48 kHz, trimming `latency` and keeping
/// `tail`.
fn plan(latency: Samples, tail: Samples) -> RenderPlan {
    RenderPlan::of(&Job::new(SampleRate(48_000.0), Samples(48_000), latency, tail).unwrap())
}

/// Note there is no graph in any of these: the plan is arithmetic, so it is
/// checked as arithmetic.
#[test]
fn no_trim_renders_exactly_the_audible_span() {
    let plan = plan(Samples(0), Samples(0));
    assert_eq!(plan.output_length, Samples(48_000));
    assert_eq!(plan.total, Samples(48_000));
    assert_eq!(plan.latency, Samples(0));
}

/// The load-bearing property: trimming N frames means rendering N extra, or
/// the file comes out short by exactly the trim.
#[test]
fn a_trim_extends_the_render_by_that_much() {
    let plan = plan(Samples(512), Samples(0));
    assert_eq!(plan.output_length, Samples(48_000));
    assert_eq!(plan.total, Samples(48_512));
    assert_eq!(plan.latency, Samples(512));
}

/// A tail extends both the frames rendered and the frames kept.
///
/// Two gates truncate a render independently, and they need opposite
/// treatment from the head trim: latency is rendered then dropped, a tail is
/// rendered then kept. Extending only `total` would pull the decay out of
/// the graph and let the sink's `output_length` cap discard it.
#[test]
fn a_tail_extends_both_what_is_rendered_and_what_is_kept() {
    let plan = plan(Samples(512), Samples(48_000));
    assert_eq!(
        plan.output_length,
        Samples(96_000),
        "the tail must survive the sink's cap"
    );
    assert_eq!(
        plan.total,
        Samples(96_512),
        "and the graph must be pulled for it, plus the trimmed head"
    );
}
