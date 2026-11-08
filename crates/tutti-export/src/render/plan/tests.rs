use super::*;

use crate::config::RenderConfig;
use tutti_types::{BeatDuration, Bpm};

#[test]
fn beats_and_seconds_agree_at_the_same_length() {
    let rate = SampleRate(48_000.0);
    // 8 beats at 120 bpm is 4 seconds.
    let beats = beats_to_seconds(BeatDuration(8.0), Bpm::of(120.0), rate);
    assert_eq!(
        duration_to_frames(beats, rate),
        duration_to_frames(4.0, rate)
    );
    assert_eq!(duration_to_frames(beats, rate), Samples(192_000));
}

/// The `f64` is load-bearing, not an oversight: an `f32` round trip at a
/// realistic derived length lands on a different frame count.
#[test]
fn long_durations_keep_sample_accuracy() {
    let rate = SampleRate(48_000.0);
    // 2000 beats at 93 bpm — an ordinary long-set length, not a round one.
    let secs = (2000.0f64 / 93.0) * 60.0;
    let exact = duration_to_frames(secs, rate);
    let via_f32 = Samples((f64::from(secs as f32) * rate.get()).round() as usize);
    assert_ne!(exact, via_f32, "this conversion must not narrow");
    assert_eq!(exact, Samples(61_935_484));
}

#[test]
fn a_non_finite_or_negative_duration_is_no_frames() {
    let rate = SampleRate(48_000.0);
    assert_eq!(duration_to_frames(-1.0, rate), Samples(0));
    assert_eq!(duration_to_frames(f64::NAN, rate), Samples(0));
    // INFINITY is the one that needs the guard. Rust's float->int cast
    // saturates, so -1.0 and NaN both reach 0 on their own and assert
    // nothing about this function; `INFINITY * rate` casts to usize::MAX,
    // which would be an unbounded render.
    assert_eq!(duration_to_frames(f64::INFINITY, rate), Samples(0));
}

fn config(latency: Samples) -> RenderConfig {
    RenderConfig {
        sample_rate: SampleRate(48_000.0),
        duration_seconds: 1.0,
        latency,
        on_event_loss: Default::default(),
        tail: Samples(0),
    }
}

/// Note there is no graph in any of these: the plan is arithmetic, so it is
/// checked as arithmetic.
#[test]
fn no_trim_renders_exactly_the_audible_span() {
    let plan = RenderPlan::new(&config(Samples(0)));
    assert_eq!(plan.output_length, Samples(48_000));
    assert_eq!(plan.total, Samples(48_000));
    assert_eq!(plan.latency, Samples(0));
}

/// The load-bearing property: trimming N frames means rendering N extra, or
/// the file comes out short by exactly the trim.
#[test]
fn a_trim_extends_the_render_by_that_much() {
    let plan = RenderPlan::new(&config(Samples(512)));
    assert_eq!(plan.output_length, Samples(48_000));
    assert_eq!(plan.total, Samples(48_512));
    assert_eq!(plan.latency, Samples(512));
}

/// A tail extends both the frames rendered and the frames kept.
///
/// Two gates truncate a render independently, and they need opposite
/// treatment from the head trim: latency is rendered then dropped, a tail is
/// rendered then kept. Extending only `total` would pull the decay out of
/// the graph and let the sink's `output_length` cap discard it — a bug no
/// arithmetic test that checked one field could see.
#[test]
fn a_tail_extends_both_what_is_rendered_and_what_is_kept() {
    let plan = RenderPlan::new(&RenderConfig {
        tail: Samples(48_000),
        ..config(Samples(512))
    });
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
