use super::*;

/// A device that supports `[min, max]` at `format`.
fn range(min: u32, max: u32, format: cpal::SampleFormat) -> cpal::SupportedStreamConfigRange {
    cpal::SupportedStreamConfigRange::new(
        2,
        cpal::SampleRate(min),
        cpal::SampleRate(max),
        cpal::SupportedBufferSize::Unknown,
        format,
    )
}

fn fallback(rate: u32) -> cpal::SupportedStreamConfig {
    range(rate, rate, cpal::SampleFormat::F32).with_sample_rate(cpal::SampleRate(rate))
}

/// **A device that can run at the graph's rate is opened at it**, not at
/// whatever its default happens to be.
///
/// Before this, `open_inner` took `default_input_config()` unconditionally.
/// A mic defaulting to 44.1 kHz on a 48 kHz graph opened at 44.1 and
/// drifted, because `MicMonitorNode` does not resample and nothing
/// compared the two rates.
///
/// Mutation-checked: returning `fallback` unconditionally fails this.
#[test]
fn a_device_covering_the_graph_rate_is_opened_at_it() {
    let chosen = choose_input_config(
        [range(8_000, 96_000, cpal::SampleFormat::F32)].into_iter(),
        fallback(44_100),
        SampleRate(48_000.0),
        "Wide Range Mic",
    )
    .expect("48k is inside [8k, 96k]");
    assert_eq!(chosen.sample_rate(), cpal::SampleRate(48_000));
}

/// **F32 wins when several supported ranges cover the rate.**
///
/// Not cosmetic: `build_input` converts every other format per sample, and
/// F32 is already the graph's type.
#[test]
fn f32_is_preferred_over_an_integer_format_at_the_same_rate() {
    let chosen = choose_input_config(
        [
            range(44_100, 48_000, cpal::SampleFormat::I16),
            range(44_100, 48_000, cpal::SampleFormat::F32),
        ]
        .into_iter(),
        fallback(44_100),
        SampleRate(48_000.0),
        "Dual Format Mic",
    )
    .expect("both ranges cover 48k");
    assert_eq!(chosen.sample_format(), cpal::SampleFormat::F32);
    assert_eq!(chosen.sample_rate(), cpal::SampleRate(48_000));
}

/// An integer-only device is still accepted — preference is not a
/// requirement.
#[test]
fn an_integer_only_device_is_accepted_at_the_graph_rate() {
    let chosen = choose_input_config(
        [range(44_100, 48_000, cpal::SampleFormat::I16)].into_iter(),
        fallback(44_100),
        SampleRate(48_000.0),
        "I16 Mic",
    )
    .expect("the range covers 48k");
    assert_eq!(chosen.sample_format(), cpal::SampleFormat::I16);
    assert_eq!(chosen.sample_rate(), cpal::SampleRate(48_000));
}

/// **A device that cannot reach the graph's rate is an error, naming both
/// rates.**
///
/// The whole point: silent drift becomes a refusal a host can show.
///
/// Mutation-checked: returning `Ok(fallback)` here fails this.
#[test]
fn a_device_that_cannot_reach_the_graph_rate_is_refused() {
    let err = choose_input_config(
        [range(44_100, 44_100, cpal::SampleFormat::F32)].into_iter(),
        fallback(44_100),
        SampleRate(48_000.0),
        "Fixed 44k1 Mic",
    )
    .expect_err("44.1k-only cannot serve a 48k graph");

    match err {
        Error::SampleRateMismatch {
            device_name,
            device,
            graph,
        } => {
            assert_eq!(device_name, "Fixed 44k1 Mic");
            assert_eq!(device.get(), 44_100.0);
            assert_eq!(graph.get(), 48_000.0);
        }
        other => panic!("expected SampleRateMismatch, got {other:?}"),
    }
}

/// A device that advertises nothing but whose default already matches is
/// accepted — some backends report an empty supported-config list.
#[test]
fn a_default_that_already_matches_is_accepted_with_no_ranges() {
    let chosen = choose_input_config(
        std::iter::empty(),
        fallback(48_000),
        SampleRate(48_000.0),
        "Silent About Its Configs",
    )
    .expect("the default already runs at the graph rate");
    assert_eq!(chosen.sample_rate(), cpal::SampleRate(48_000));
}
