use super::*;

use crate::ConfigError;
use crate::config::RenderConfig;
use tutti_types::{BeatDuration, Bpm, Seconds};

/// The plan of a config's job.
fn plan_of(config: &RenderConfig) -> Result<RenderPlan, ConfigError> {
    config.job().map(|job| RenderPlan::of(&job))
}

#[test]
fn beats_and_seconds_agree_at_the_same_length() {
    let rate = SampleRate(48_000.0);
    // 8 beats at 120 bpm is 4 seconds.
    let beats = beats_to_seconds(BeatDuration(8.0), Bpm::of(120.0), rate);
    assert_eq!(
        duration_to_frames(beats, rate),
        duration_to_frames(Seconds(4.0), rate)
    );
    assert_eq!(duration_to_frames(beats, rate), Samples(192_000));
}

/// The `f64` width of `Seconds` is load-bearing: an `f32` round trip at a
/// realistic derived length lands on a different frame count.
#[test]
fn long_durations_keep_sample_accuracy() {
    let rate = SampleRate(48_000.0);
    // 2000 beats at 93 bpm — an ordinary long-set length, not a round one.
    let secs = (2000.0f64 / 93.0) * 60.0;
    let exact = duration_to_frames(Seconds(secs), rate);
    let via_f32 = Samples((f64::from(secs as f32) * rate.get()).round() as usize);
    assert_ne!(exact, via_f32, "this conversion must not narrow");
    assert_eq!(exact, Samples(61_935_484));
}

#[test]
fn a_non_finite_or_negative_duration_is_no_frames() {
    let rate = SampleRate(48_000.0);
    assert_eq!(duration_to_frames(Seconds(-1.0), rate), Samples(0));
    assert_eq!(duration_to_frames(Seconds(f64::NAN), rate), Samples(0));
    // INFINITY is the one that needs the guard. Rust's float->int cast
    // saturates, so -1.0 and NaN both reach 0 on their own and assert
    // nothing about this function; `INFINITY * rate` casts to usize::MAX,
    // which would be an unbounded render.
    assert_eq!(duration_to_frames(Seconds(f64::INFINITY), rate), Samples(0));
}

fn config(latency: Samples) -> RenderConfig {
    RenderConfig {
        sample_rate: SampleRate(48_000.0),
        duration_seconds: Seconds(1.0),
        latency,
        on_event_loss: Default::default(),
        tail: Samples(0),
    }
}

/// Note there is no graph in any of these: the plan is arithmetic, so it is
/// checked as arithmetic.
#[test]
fn no_trim_renders_exactly_the_audible_span() {
    let plan = plan_of(&config(Samples(0))).unwrap();
    assert_eq!(plan.output_length, Samples(48_000));
    assert_eq!(plan.total, Samples(48_000));
    assert_eq!(plan.latency, Samples(0));
}

/// The load-bearing property: trimming N frames means rendering N extra, or
/// the file comes out short by exactly the trim.
#[test]
fn a_trim_extends_the_render_by_that_much() {
    let plan = plan_of(&config(Samples(512))).unwrap();
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
    let plan = plan_of(&RenderConfig {
        tail: Samples(48_000),
        ..config(Samples(512))
    })
    .unwrap();
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

/// A duration that is not a length is a configuration error, not an empty
/// render. (`duration_to_frames` alone maps all of these to 0 frames, which is
/// why the plan asks first.)
///
/// Mutation (run): drop the `is_finite`/sign check in `RenderConfig::job` → NaN,
/// infinity and -1 all reach `EmptyRender` (or, for infinity, a render of
/// `usize::MAX` frames saturated to 0 by `duration_to_frames`) → fails.
#[test]
fn a_duration_that_is_not_a_length_is_refused() {
    for seconds in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, -0.0001] {
        let config = RenderConfig {
            duration_seconds: Seconds(seconds),
            ..config(Samples(0))
        };
        assert_eq!(
            plan_of(&config),
            Err(ConfigError::InvalidDuration),
            "{seconds}"
        );
    }
}

/// A plan that keeps nothing is refused, whatever it trims: no duration (the
/// default's), one shorter than a frame, with or without a latency to trim.
/// A tail alone is a render.
///
/// Mutation (run): drop the `output_length == 0` check → the default config
/// plans a zero-frame render → fails.
#[test]
fn a_render_of_no_frames_is_refused() {
    let none = RenderConfig {
        duration_seconds: Seconds(0.0),
        ..config(Samples(512))
    };
    assert_eq!(plan_of(&none), Err(ConfigError::EmptyRender));
    assert_eq!(
        plan_of(&RenderConfig::default()),
        Err(ConfigError::EmptyRender)
    );
    // Half a frame at 48 kHz rounds to nothing (0.4 frames).
    let sub_frame = RenderConfig {
        duration_seconds: Seconds(0.4 / 48_000.0),
        ..none
    };
    assert_eq!(plan_of(&sub_frame), Err(ConfigError::EmptyRender));
    let tail_only = RenderConfig {
        tail: Samples(10),
        ..none
    };
    assert_eq!(plan_of(&tail_only).unwrap().output_length, Samples(10));
}
