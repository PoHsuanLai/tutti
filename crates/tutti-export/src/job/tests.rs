//! `Job::new`'s refusals, each through the public constructor with its named
//! error, and what a `Job` reads back.

use crate::{EventLossPolicy, Job, JobError, MAX_FRAMES};
use tutti_types::{SampleRate, Samples};

const RATE: SampleRate = SampleRate(48_000.0);

fn job(length: usize, latency: usize, tail: usize) -> Result<Job, JobError> {
    Job::new(RATE, Samples(length), Samples(latency), Samples(tail))
}

/// A rate that is NaN or infinite is refused, and the error carries it.
///
/// Mutation (run): `is_rate` without the `is_finite` check (`rate > 0.0`
/// only) -> infinity is accepted -> fails.
#[test]
fn a_non_finite_rate_is_refused() {
    for rate in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let err = Job::new(SampleRate(rate), Samples(1), Samples(0), Samples(0)).unwrap_err();
        assert!(
            matches!(err, JobError::InvalidRate { .. }),
            "{rate}: {err:?}"
        );
    }
}

/// A rate of zero or below is refused.
///
/// Mutation (run): `is_rate` with `>= 0.0` -> zero is accepted -> fails;
/// without the sign check at all -> the negative rows fail.
#[test]
fn a_non_positive_rate_is_refused() {
    for rate in [0.0, -0.0, -1.0, -48_000.0] {
        let err = Job::new(SampleRate(rate), Samples(1), Samples(0), Samples(0)).unwrap_err();
        assert!(
            matches!(err, JobError::InvalidRate { .. }),
            "{rate}: {err:?}"
        );
    }
}

/// A job with no audible frame is refused, whatever it trims or keeps.
///
/// Mutation (run): drop the `length == 0` check -> `Ok` -> fails.
#[test]
fn a_zero_length_is_refused() {
    for (latency, tail) in [(0, 0), (512, 0), (0, 10), (512, 10)] {
        assert_eq!(job(0, latency, tail), Err(JobError::ZeroLength));
    }
}

/// A total that overflows a frame count is refused, not wrapped.
///
/// Mutation (run): `checked_add` replaced by a wrapping add in `new` -> the
/// sum wraps to a small figure and the job is accepted -> fails.
#[test]
fn a_total_that_overflows_is_refused() {
    let err = job(usize::MAX, 1, 0).unwrap_err();
    assert_eq!(
        err,
        JobError::TooLong {
            total: Samples(usize::MAX)
        }
    );
    assert!(matches!(
        job(1, usize::MAX, usize::MAX),
        Err(JobError::TooLong { .. })
    ));
}

/// A total past `MAX_FRAMES` is refused with the figure it came to, and
/// exactly `MAX_FRAMES` is a job: latency, length and tail all count.
///
/// Mutation (run): `n <= MAX_FRAMES` -> `n < MAX_FRAMES` makes the boundary
/// row fail; leaving `latency` out of the sum makes the latency row fail.
#[test]
fn a_total_past_the_most_frames_is_refused() {
    assert!(job(MAX_FRAMES, 0, 0).is_ok(), "the bound itself is allowed");
    assert!(job(MAX_FRAMES - 30, 10, 20).is_ok());
    assert_eq!(
        job(MAX_FRAMES, 0, 1),
        Err(JobError::TooLong {
            total: Samples(MAX_FRAMES + 1)
        })
    );
    assert!(matches!(
        job(MAX_FRAMES - 30, 11, 20),
        Err(JobError::TooLong { .. })
    ));
    assert!(matches!(
        job(MAX_FRAMES - 30, 10, 21),
        Err(JobError::TooLong { .. })
    ));
}

/// The getters read back what was given, the derived counts add as documented
/// and the event-loss policy defaults to failing.
///
/// Mutation (run): `output_length` leaving out the tail, or `total` leaving
/// out the latency -> the figures below fail.
#[test]
fn a_job_reads_back_what_it_was_given() {
    let job = job(1_000, 64, 200).unwrap();
    assert_eq!(job.rate(), RATE);
    assert_eq!(job.length(), Samples(1_000));
    assert_eq!(job.latency(), Samples(64));
    assert_eq!(job.tail(), Samples(200));
    assert_eq!(job.output_length(), Samples(1_200));
    assert_eq!(job.total(), Samples(1_264));
    assert_eq!(job.on_event_loss(), EventLossPolicy::Fail);
    let allowed = job.with_event_loss(EventLossPolicy::Allow);
    assert_eq!(allowed.on_event_loss(), EventLossPolicy::Allow);
    assert_eq!(allowed.total(), job.total());
}
