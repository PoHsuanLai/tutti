//! Every figure is `Instant` arithmetic on a constructed `ClockMap`: the plan
//! reads no clock, so `base` is any instant and only differences matter.

use serde::{Deserialize, Serialize};
use tutti_fixtures::{RangeContract, check_range_contract};

use super::*;
use crate::ClockReading;
use tutti_types::SampleRate;

const R48: SampleRate = SampleRate::SR_48K;

fn base() -> Instant {
    Instant::now() + Duration::from_secs(3600)
}

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

/// A map where frame 0 is at `t0`.
fn map_at(t0: Instant) -> ClockMap {
    ClockMap::new(R48, ClockReading::new(t0, Frame(0)))
}

fn terms(audio: Option<Duration>, device: Duration, offset: f64) -> OutputTerms {
    OutputTerms {
        audio_output: audio,
        latency: OutputLatency::new(device, OutputOffset::new(Seconds(offset)).unwrap()).unwrap(),
    }
}

/// **A frame is sent at the instant it is heard, plus the audio latency, less
/// the device's latency, plus the offset.** Frame 4 800 is 100 ms after the
/// anchor; 30 ms of audio latency, 5 ms of device latency and a 0.25 s offset
/// make it 100 + 30 - 5 + 250 ms.
#[test]
fn a_frame_is_sent_at_heard_plus_audio_less_device_plus_offset() {
    let t0 = base();
    let plan = OutputPlan::new(terms(Some(ms(30)), ms(5), 0.25));
    assert_eq!(
        plan.place(Frame(4_800), Some(&map_at(t0)), t0 - ms(10)),
        Placement::Send(SendAt::At(t0 + ms(100 + 30 - 5 + 250)))
    );
}

/// **A negative offset sends earlier.** -0.25 s moves a frame 0.5 s out to
/// 250 ms.
#[test]
fn a_negative_offset_sends_earlier() {
    let t0 = base();
    let plan = OutputPlan::new(terms(Some(Duration::ZERO), Duration::ZERO, -0.25));
    assert_eq!(
        plan.place(Frame(24_000), Some(&map_at(t0)), t0 - ms(10)),
        Placement::Send(SendAt::At(t0 + ms(250)))
    );
}

/// **A send time at or before now is sent at once**, and so is one the clock
/// puts in the past.
#[test]
fn a_time_at_or_before_now_is_sent_now() {
    let t0 = base();
    let plan = OutputPlan::new(terms(Some(ms(10)), Duration::ZERO, 0.0));
    let map = map_at(t0);
    // Frame 4 800 is at t0 + 110 ms.
    assert_eq!(
        plan.place(Frame(4_800), Some(&map), t0 + ms(110)),
        Placement::Send(SendAt::Now),
        "exactly now"
    );
    assert_eq!(
        plan.place(Frame(4_800), Some(&map), t0 + ms(500)),
        Placement::Send(SendAt::Now),
        "past"
    );
    assert_eq!(
        plan.place(Frame(4_800), Some(&map), t0 + ms(109)),
        Placement::Send(SendAt::At(t0 + ms(110))),
        "1 ms ahead is scheduled"
    );
}

/// **A send time more than the cap ahead is refused, one at the cap is not.**
#[test]
fn a_time_beyond_the_cap_is_refused() {
    let t0 = base();
    let plan = OutputPlan::default();
    let map = map_at(t0);
    let frames_at = |d: Duration| Frame((d.as_secs_f64() * 48_000.0) as u64);
    assert_eq!(
        plan.place(frames_at(MAX_OUTPUT_AHEAD + ms(1)), Some(&map), t0),
        Placement::TooFar
    );
    assert_eq!(
        plan.place(frames_at(MAX_OUTPUT_AHEAD), Some(&map), t0),
        Placement::Send(SendAt::At(t0 + MAX_OUTPUT_AHEAD))
    );
}

/// **A frame the clock cannot place is sent now**: no map, and a frame before
/// the origin of the clock's instants.
#[test]
fn a_frame_the_clock_cannot_place_is_sent_now() {
    let t0 = base();
    let plan = OutputPlan::new(terms(Some(ms(30)), Duration::ZERO, 0.0));
    assert_eq!(
        plan.place(Frame(4_800), None, t0),
        Placement::Send(SendAt::Now)
    );
    let tiny = SampleRate::of(1e-9);
    let slow = ClockMap::new(tiny, ClockReading::new(t0, Frame(0)));
    assert_eq!(
        plan.place(Frame(u64::MAX), Some(&slow), t0),
        Placement::Send(SendAt::Now),
        "an instant too far to hold"
    );
}

/// **An unreported audio latency is not a zero.** With the term unknown the
/// terms are incomplete and the send time omits only that term; once it is
/// known the term is in, by exactly its value.
#[test]
fn an_unreported_audio_latency_omits_only_that_term() {
    let t0 = base();
    let map = map_at(t0);
    let now = t0 - ms(10);
    let unknown = OutputPlan::new(terms(None, ms(5), 0.0));
    let known = OutputPlan::new(terms(Some(ms(30)), ms(5), 0.0));
    assert!(!unknown.terms().is_complete());
    assert!(known.terms().is_complete());
    assert_eq!(
        unknown.place(Frame(4_800), Some(&map), now),
        Placement::Send(SendAt::At(t0 + ms(100 - 5))),
        "everything but the audio latency"
    );
    assert_eq!(
        known.place(Frame(4_800), Some(&map), now),
        Placement::Send(SendAt::At(t0 + ms(100 + 30 - 5))),
        "the audio latency, as reported, in"
    );
    assert!(!OutputTerms::default().is_complete());
}

// `OutputOffset` has no serde in the library (nothing persists one); the
// contract checks the `try_from` shape written here, as `EventCapacity`'s does.
impl Serialize for OutputOffset {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.get().get().serialize(s)
    }
}

impl<'de> Deserialize<'de> for OutputOffset {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::try_from(Seconds(f64::deserialize(d)?)).map_err(serde::de::Error::custom)
    }
}

/// **An offset is range-checked at `new`:** a second either way is held, more
/// than that and anything not finite is refused, never clamped.
#[test]
fn an_output_offset_keeps_the_range_checked_shape() {
    check_range_contract(&RangeContract {
        new: OutputOffset::new,
        get: OutputOffset::get,
        of: None,
        valid: &[Seconds(0.0), Seconds(-1.0), Seconds(1.0), Seconds(0.0125)],
        invalid: &[
            Seconds(1.000_001),
            Seconds(-1.5),
            Seconds(f64::NAN),
            Seconds(f64::INFINITY),
        ],
    });
}

/// A device latency over the same bound is refused.
#[test]
fn a_device_latency_beyond_a_second_is_refused() {
    assert!(OutputLatency::new(ms(1000), OutputOffset::ZERO).is_some());
    assert!(OutputLatency::new(ms(1001), OutputOffset::ZERO).is_none());
}
