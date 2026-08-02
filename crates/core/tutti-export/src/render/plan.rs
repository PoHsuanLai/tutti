//! Frame-count arithmetic for one offline render.
//!
//! Derived once from (duration, rate, latency), and drives both how many frames
//! the net must produce and how many leading frames the sink drops.

use crate::config::RenderConfig;
use tutti_core::SampleRate;
use tutti_types::{BeatDuration, Bpm, Samples};

/// Fixed scheduling parameters for one render.
///
/// Every field is [`Samples`] — a discrete frame count, which is what the type
/// exists for (its own doc names the compensation-delay path). Note that
/// `Samples` has no `Sub`: use `remaining_after` / `align_to`, which is why the
/// driver reads the way it does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RenderPlan {
    /// Frames the net must produce, including latency slack when trimming.
    pub total: Samples,
    /// Frames the sink keeps: the requested span plus the tail.
    pub output_length: Samples,
    /// Leading frames the sink drops.
    pub latency: Samples,
}

impl RenderPlan {
    /// Derive the plan from a config. **Pure** — arithmetic over three numbers.
    ///
    /// It used to take `&mut tutti_core::dsp::Net`, for one reason: resolving a
    /// `LatencyTrim::Reported` variant by calling `net.latency()`. One mode on
    /// one field made a frame-count calculation require a mutable audio graph,
    /// which meant it could not be tested, reused, or reasoned about without
    /// building a graph first. The caller resolves the latency now (see
    /// [`reported_latency`](crate::reported_latency)) and passes a number.
    pub fn new(config: &RenderConfig) -> Self {
        let duration = duration_to_frames(config.duration_seconds, config.sample_rate);
        // The tail extends what the sink KEEPS, not just what the net produces.
        // Two gates truncate independently — `total` bounds the pull loop and
        // `output_length` caps the kept frames — so extending only the first
        // would render the decay and then discard it.
        let output_length = duration + config.tail;
        // Render the kept span PLUS the trimmed head, so the output is still
        // `output_length` frames long after the drop.
        // Both are frame counts, so this is `Samples`' own saturating `Add`
        // rather than an unwrapped `usize` one.
        let total = output_length + config.latency;

        Self {
            total,
            output_length,
            latency: config.latency,
        }
    }
}

/// Frames a `seconds` span covers at `rate`.
///
/// The one duration conversion in the crate, so the rounding happens once. A
/// non-finite or negative span is no frames rather than a panic or a wrapped
/// length.
///
/// # Why `f64` and not [`Seconds`](tutti_types::Seconds)
///
/// The crate's one stop short of a unit type, and CLAUDE.md names the case:
/// *"`Seconds` is f32, so SMPTE timecode and hour-long render durations stay
/// f64."* Concretely, `Seconds` resolves individual frames only to ~256 s at
/// 48 kHz. Round lengths survive anyway (`3600.0` is exact), but a *derived* one
/// does not — 2000 beats at 93 bpm lands 2 frames off, 8000 at 111 bpm lands 6.
/// Small, silent, and every hand-written test would use a round number and pass.
pub fn duration_to_frames(seconds: f64, rate: SampleRate) -> Samples {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Samples(0);
    }
    Samples((seconds * rate.get()).round() as usize)
}

/// Seconds covering `len` beats at `tempo` — the musical-vocabulary form.
///
/// Routed through [`beats_per_sample`](tutti_core::transport::beats_per_sample)
/// rather than `BeatDuration::to_seconds`, which returns `f32` `Seconds` and
/// would reintroduce the narrowing above. The association `(tempo / 60) / rate`
/// is load-bearing — see that function.
pub fn beats_to_seconds(len: BeatDuration, tempo: Bpm, rate: SampleRate) -> f64 {
    let bps = tutti_core::transport::beats_per_sample(tempo, rate).get();
    if bps <= 0.0 {
        return 0.0;
    }
    (len.get() / bps) / rate.get()
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::RenderConfig;
    use tutti_types::{BeatDuration, Bpm};

    #[test]
    fn beats_and_seconds_agree_at_the_same_length() {
        let rate = SampleRate(48_000.0);
        // 8 beats at 120 bpm is 4 seconds.
        let beats = beats_to_seconds(BeatDuration(8.0), Bpm(120.0), rate);
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
            tail: Samples(0),
        }
    }

    /// Note there is no `Net` in any of these. That is the point of the change:
    /// the plan is arithmetic, so it can be checked as arithmetic.
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
            "and the net must be pulled for it, plus the trimmed head"
        );
    }

    /// A zero tail is exactly the arithmetic from before tails existed.
    #[test]
    fn no_tail_leaves_the_plan_unchanged() {
        let plan = RenderPlan::new(&config(Samples(512)));
        assert_eq!(plan.output_length, Samples(48_000));
        assert_eq!(plan.total, Samples(48_512));
    }
}
